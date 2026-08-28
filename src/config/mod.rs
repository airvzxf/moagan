//! Configuration model and loader.
//!
//! Resolution order (highest priority first):
//! 1. CLI flags (wired in commit 10).
//! 2. `MOAGAN_*` environment variables (e.g. `MOAGAN_MAX_PARALLELISM`).
//! 3. `~/.config/moagan/config.toml` if present.
//! 4. Built-in defaults.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::llm::prompts::DEFAULT_MAX_TOKENS;
use crate::sandbox::process::NamespaceFlags;
use crate::sandbox::{CgroupLimits, NetworkPolicy, SeccompPolicyKind};

pub mod profile;
pub use profile::Profile;

/// Top-level configuration record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where runs and meta live. Defaults to `${MOAGAN_HOME:-~/.local/share/moagan}`.
    pub home: Option<PathBuf>,
    /// Hard cap on concurrent LLM calls. Default 4.
    pub max_parallelism: usize,
    /// Sketch timeout in seconds. 0 means infinite. Default 120.
    pub sketch_timeout_secs: u64,
    /// Phase timeout in seconds. 0 means infinite. Default 0.
    pub phase_timeout_secs: u64,
    /// Total run timeout in seconds. 0 means infinite. Default 0.
    pub total_timeout_secs: u64,
    /// Token budget for the run. `None` means unlimited.
    pub token_budget: Option<u64>,
    /// Named providers the user can select with `--provider`.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Default provider name (`SECTION[:MODEL]`) when the operator
    /// omits `--provider`. Empty string = "no default"; the CLI
    /// falls back to `MOAGAN_DEFAULT_PROVIDER` then to a clear
    /// error message. v0.10 keeps the field as `String` for
    /// backwards compatibility with v0.9 TOML files; the `Option`
    /// shape is encoded as `""` (empty) for the missing case.
    pub default_provider: String,
    /// Default export format. `tar.gz` or `zip`.
    pub export_format: String,
    /// Export compression level (0..=9). Default 6.
    pub export_compression: u32,
    /// Redact secrets before persisting. Default true.
    pub redact_in_telemetry: bool,
    /// Include run summary in CI logs. Default false.
    pub emit_summary: bool,
    /// Disable colour in CLI output. Default false.
    pub no_color: bool,
    /// Per-criterion weights for the ranked score. Equal weights by default.
    pub ranking_weights: RankingWeights,
    #[allow(missing_docs)]
    pub rubric: RubricConfig,
    #[allow(missing_docs)]
    #[serde(default)]
    pub llm: LlmConfig,
    /// Track E (E7 partial): knobs for the Critique phase. Today the
    /// only knob is the opt-in switch for the `TiefighterCritic`
    /// adversarial cross-check sidecar — the default is `false` so
    /// existing runs are unaffected. Operators opt in via
    /// `MOAGAN_CRITIQUE_TIEFIGHTER_ENABLED=true` or by setting
    /// `[critique]\ntiefighter_enabled = true` in
    /// `~/.config/moagan/config.toml`. The sidecar path is
    /// `<run_dir>/critiques/<proposal_id>_tiefighter.json` and the
    /// verdict is mirrored onto `Proposal::tiefighter_score` for
    /// downstream phases that want to filter without re-reading the
    /// sidecar.
    pub critique: CritiqueConfig,
    /// Maximum number of repair rounds per failed proposal. Default 5.
    /// Spec T01-06 §16.10 allows 0..2; v0.1 default is 5 per operator
    /// preference (the cost is bounded by parallelism).
    pub repair_max_rounds: u32,
    /// Forbidden technologies the gate rejects when present in a
    /// proposal. Empty by default; the user can populate this from
    /// the brief's constraints.
    pub gate_forbidden_techs: Vec<String>,
    /// Minimum proposal length (chars) for the gate length check.
    pub gate_min_length: usize,
    /// Maximum proposal length (chars) for the gate length check.
    pub gate_max_length: usize,
    /// Per-role temperature overrides sourced from the active
    /// domain profile. Populated by `Config::apply_profile`; key
    /// is the role name (e.g. `rust_competitive`), value is the
    /// sampling temperature. Phases consult this map when present.
    #[serde(default)]
    pub profile_temperature_overrides: std::collections::HashMap<String, f32>,
    /// Per-mode judge quorum overrides sourced from the active
    /// domain profile. Populated by `Config::apply_profile`; key
    /// is the mode name (e.g. `fast`), value is the desired judge
    /// count for that mode.
    #[serde(default)]
    pub profile_judge_quorum_overrides: std::collections::HashMap<String, usize>,
    /// Phase H: knobs for the ranking-stability check. See
    /// [`StabilityConfig`] for the field semantics. Default config
    /// runs 8 perturbations at sigma 0.05 (non-interactive) / 0.10
    /// (interactive) and labels the ranking `Sensitive` when the
    /// top-1 winner held its position in fewer than 80% of the
    /// perturbations.
    pub stability: StabilityConfig,
    /// Phase I (v0.3 sub-fase I): knobs for `moagan telemetry view`
    /// (the read-only HTTP dashboard). `DEFAULT_PORT` per
    /// `proposal-01-concept.md §8.8`.
    pub server: ServerConfig,
    /// Phase I (v0.3 sub-fase I): knobs for `moagan telemetry
    /// cleanup` (the retention pass). Mirrors the catalog
    /// `D.5.1` retention knobs.
    pub retention: RetentionConfig,
    /// Per-provider circuit breaker (catalog 10-integrada-v0 §D.19.5,
    /// T00-08 §1428-1435). Five opening errors inside `window_secs`
    /// sideline the provider for `cooldown_secs`. The wrapper that
    /// fronts every provider in the registry consults
    /// [`crate::Error::is_circuit_opening`] before recording a
    /// failure so non-opening errors (schema, operator, cancel) do
    /// not consume the budget.
    pub circuit_breaker: CircuitBreakerConfig,
    /// Track F: whether `moagan run` / `moagan continue` /
    /// `moagan discover` should auto-run the reconcile pass at
    /// startup (D.28.3 + D.28.4: cleanup atomic-write leftovers
    /// and recover zombie `running` runs). Default `true`.
    /// Overridable via `MOAGAN_STARTUP_RECONCILE=false`.
    #[serde(default = "default_startup_reconcile")]
    pub startup_reconcile: bool,
    /// Track E (catalog §D.11.9): allow the sandbox subprocess to
    /// reach the network. Default `false` (off-by-default) so the
    /// default install never silently contacts the registry / an
    /// arbitrary host. Operators opt in via
    /// `MOAGAN_SANDBOX_ALLOW_NETWORK=true` or by setting this in
    /// `~/.config/moagan/config.toml`. When `false`, the sandbox
    /// injects `CARGO_NET_OFFLINE=true` in the subprocess env.
    pub sandbox_allow_network: bool,
    /// Track E (catalog §D.11.13): typed network policy for the
    /// sandbox subprocess. Replaces the boolean
    /// [`Self::sandbox_allow_network`] for callers that need the
    /// `AllowList(Vec<String>)` case. Defaults to
    /// [`NetworkPolicy::Off`]. Operators opt in via
    /// `MOAGAN_SANDBOX_NETWORK_POLICY=open` (or `=allow_list` /
    /// `=["host1","host2"]`) or by setting
    /// `sandbox_network_policy = { kind = "open" }` in
    /// `~/.config/moagan/config.toml`. When the policy is `Off`,
    /// the sandbox injects `CARGO_NET_OFFLINE=true`; for `Open` and
    /// `AllowList` the hint is not injected and the pre-execution
    /// host validation is enforced (catalog §D.11.13).
    #[serde(default)]
    pub sandbox_network_policy: NetworkPolicy,
    /// Track E (catalog §D.11.10): allow the sandbox to skip the
    /// secret-stripping pass over argv. Default `false` (strip).
    /// Operators opt in via `MOAGAN_SANDBOX_ALLOW_INJECTION=true` or
    /// `moagan run --allow-injection`. When `false`, the sandbox
    /// runs `strip_secrets` over argv before spawning; when `true`,
    /// raw args are passed to the subprocess verbatim.
    pub sandbox_allow_injection: bool,
    /// Track E (catalog §D.11.2): opt-in Linux namespace isolation
    /// for sandbox subprocesses. Defaults to no namespaces so existing
    /// runs are unaffected. Operators select a comma-separated list
    /// through `MOAGAN_SANDBOX_NAMESPACES=mount,pid,net` or set
    /// `sandbox_namespaces` in `config.toml`.
    #[serde(default)]
    pub sandbox_namespaces: NamespaceFlags,
    /// Track E (catalog §D.11.7): opt-in seccomp syscall whitelist
    /// for the sandbox subprocess. Default
    /// [`SeccompPolicyKind::Permissive`] (no-op). Operators opt in
    /// via `MOAGAN_SANDBOX_SECCOMP=strict_rust_build` or by setting
    /// `sandbox_seccomp = "strict_rust_build"` in
    /// `~/.config/moagan/config.toml`. The sandbox installs the BPF
    /// program in the child's `pre_exec` hook so only the spawned
    /// subprocess is filtered.
    #[serde(default)]
    pub sandbox_seccomp: SeccompPolicyKind,
    /// Track E (catalog §D.11.1): opt-in cgroup v2 resource
    /// isolation for the sandbox subprocess. `None` means no
    /// kernel-level resource cap (the default so existing runs are
    /// unaffected). Operators opt in via
    /// `MOAGAN_SANDBOX_CGROUP=enabled` (with the canonical default
    /// profile) or by setting `sandbox_cgroup = { cpu_max = "...",
    /// memory_max_bytes = ..., pids_max = ... }` in
    /// `~/.config/moagan/config.toml`. The sandbox creates the
    /// child cgroup in its `pre_exec` hook; when cgroup v2 is
    /// unavailable it falls back to per-process `libc::prlimit`.
    #[serde(default)]
    pub sandbox_cgroup: Option<CgroupLimits>,
    /// Track K (D9): opt-in switch for the bounded external research
    /// fetcher wired into the Sketch phase. When `false` (the
    /// default) the fn never runs even if URLs are configured. When
    /// `true`, the Sketch phase fetches up to
    /// [`crate::research::MAX_URLS_PER_CALL`] URLs from
    /// `research_urls` and injects the snippets via the
    /// `${known_apis}` placeholder. Overridable via
    /// `MOAGAN_RESEARCH_ENABLED=true`.
    #[serde(default)]
    pub research_enabled: bool,
    /// Track K (D9): CSV list of URLs the research fetcher is
    /// permitted to query when [`Self::research_enabled`] is true.
    /// Hosts are filtered against the
    /// [`crate::research::ALLOWED_HOSTS`] allowlist so a CSV
    /// element does not silently smuggle an attacker-controlled host
    /// into the prompt. Default `[]`. Overridable via
    /// `MOAGAN_RESEARCH_URLS=docs.rs/foo,crates.io/bar`.
    #[serde(default)]
    pub research_urls: Vec<String>,
    /// Track K (D9): research sub-knobs grouped under `research`.
    /// Holds the optional bearer token applied to hosts whose
    /// [`crate::research::allowlist::HostPolicy::auth_bearer`] flag
    /// is `true` (currently `api.github.com`). Operators opt in
    /// via `MOAGAN_RESEARCH_API_KEY=<token>` or by setting
    /// `[research]\napi_key = "..."` in
    /// `~/.config/moagan/config.toml`. The key never appears in
    /// the serialized output or any CLI flag dump — it stays
    /// inside the `Config` and feeds straight into the
    /// `Authorization: Bearer ...` header at fetch time.
    #[serde(default)]
    pub research: ResearchConfig,
    /// Track E (catalog §D.19.6): per-provider token-bucket knobs.
    /// Empty by default; opt in via
    /// `MOAGAN_RATE_LIMIT_<provider>=<capacity>:<refill_per_sec>` or by
    /// setting `[rate_limit_per_provider]` in `~/.config/moagan/config.toml`.
    /// Each entry is consumed by `BreakeredProvider::send` so calls
    /// beyond `capacity` either wait for the next refill or fail with
    /// an `Error::Provider` (carrying a budget-exhausted message)
    /// when a max-wait is configured. Provider responses whose
    /// `Usage::cache_read > 0` refund the token so the upstream
    /// prompt cache does not drain the local bucket.
    #[serde(default)]
    pub rate_limit_per_provider: std::collections::HashMap<String, RateLimitConfig>,
    /// Track E (catalog §D.19.6): per-role token-bucket knobs.
    /// Same shape as `rate_limit_per_provider` but keyed by the
    /// `Role::as_str()` value (e.g. `"tagger"`, `"facet_deriver"`,
    /// `"extractor"`). Empty by default = no per-role limit, only the
    /// per-provider bucket applies. Opt in via env
    /// `MOAGAN_RATE_LIMIT_ROLE_<role>=<capacity>:<refill_per_sec>` or
    /// by setting `[rate_limit_per_role]` in
    /// `~/.config/moagan/config.toml`.
    ///
    /// Why: the upstream provider has its own rate-limit window that
    /// is tighter than the per-provider bucket for chatty roles
    /// like `tagger` (1500+ LLM calls per fan-out). Forcing the
    /// per-role bucket is the only way to throttle the call rate
    /// below the provider's quota without affecting the rest of the
    /// pipeline. Once acquired, the per-role limiter sleeps the
    /// caller — the same "throttle" effect the per-provider bucket
    /// has, but at a per-role granularity so the operator can
    /// target just the role that needs it.
    #[serde(default)]
    pub rate_limit_per_role: std::collections::HashMap<String, RateLimitConfig>,
    /// v0.9.6: per-`role` adaptive throttle governor knobs
    /// (AIMD backpressure for transient 429s). Same shape as
    /// `rate_limit_per_role` but consumed by
    /// [`crate::llm::governor::ThrottleGovernor`] instead of
    /// [`crate::llm::rate_limiter::RateLimiter`]. The two are
    /// complementary: the bucket caps the rate, the governor
    /// reacts to the upstream's 429 stream. Empty by default =
    /// no governor; the call-site creates a default-config
    /// governor the first time a role is invoked, so a fresh
    /// installation behaves like v0.9.5 (no adaptive
    /// backpressure, but with the new error-classification
    /// making `Throttled` go to the governor lane rather than
    /// the breaker). Opt-in via `[throttle_per_role]` in
    /// `~/.config/moagan/config.toml` or
    /// `MOAGAN_THROTTLE_PER_ROLE_<role>=<initial>:<max>:<init_backoff>:<max_backoff>:<additive_after>:<jitter>`.
    #[serde(default)]
    pub throttle_per_role: std::collections::HashMap<String, ThrottleConfig>,
    /// v0.9.6: per-`role` circuit breaker knobs. Same shape as
    /// the per-provider breaker that v0.9.4 deprecated, but
    /// keyed on `(provider, role)` (provider is `default_provider`
    /// at the call-site) instead of `(provider)` so a `PlanExhausted`
    /// on `minimax`/`facet_deriver` does not trip
    /// `minimax`/`tagger`. Empty by default = no breaker; the
    /// call-site creates a default-config breaker the first time
    /// a role is invoked. Opt-in via `[circuit_breaker_per_role]`
    /// in `~/.config/moagan/config.toml` or
    /// `MOAGAN_CIRCUIT_BREAKER_PER_ROLE_<role>=<threshold>:<window_secs>:<cooldown_secs>`.
    #[serde(default)]
    pub circuit_breaker_per_role: std::collections::HashMap<String, BreakerConfig>,
    /// Track E (E8 partial): knobs for the two D.7.1 catalog
    /// roles that the Discovery coordinator can invoke —
    /// `Role::PersonaPicker` and `Role::AnglePicker`. Both are
    /// opt-in; the helpers in `src/discovery/persona_angle.rs`
    /// short-circuit to `Ok(None)` when the corresponding switch
    /// is `false`. Operators opt in via
    /// `MOAGAN_DISCOVERY_PERSONA_ENABLED=true` /
    /// `MOAGAN_DISCOVERY_ANGLE_ENABLED=true` or by setting
    /// `[discovery]\npersona_enabled = true` in
    /// `~/.config/moagan/config.toml`. The defaults are `false`
    /// so existing runs are bit-identical.
    #[serde(default)]
    pub discovery: DiscoveryWiringConfig,
    /// PR-D1: persistent per-provider temperature-profile
    /// overrides for the discovery matrix (the CLI
    /// `--temperature-profile` flag wins on conflict). Lives
    /// under `[discovery_matrix]` in
    /// `~/.config/moagan/config.toml`. Default empty so a fresh
    /// installation matches v0.5's single-shot behaviour.
    #[serde(default)]
    pub discovery_matrix: DiscoveryMatrixConfig,
    /// Track J (D.21.3): selection strategy the rank phase
    /// applies after the weighted sort to choose which
    /// `(proposal_id, score, Proposal)` triples make it into the
    /// final `ranking.json`. Spec D.21.3 / §D.12.4. Three
    /// constructors — `SelectionPlan::keep_top`,
    /// `SelectionPlan::keep_diverse`,
    /// `SelectionPlan::keep_outlier` — produce the three
    /// flavours the catalog describes. Default
    /// `SelectionPlan::keep_top(10)` matches the spec baseline
    /// for `Mode::Deep`. Operators wanting the spread (D.7.1
    /// `explore` semantics) set
    /// `MOAGAN_SELECTION_PLAN={kind="diverse", count=15}` in
    /// `~/.config/moagan/config.toml`; the env-var override is
    /// applied in [`Self::apply_env_overrides`].
    #[serde(default = "default_selection_plan")]
    pub selection_plan: crate::phases::cardinality::SelectionPlan,
    /// Export-side knobs. The hash algorithm threads through
    /// the cache key builder
    /// (`crate::llm::wire::build_cache_key`) and the brief
    /// dual-hash helper
    /// (`crate::phases::decompose::compute_brief_hash`).
    /// `HashAlgo::Blake3` matches the canonical internal hash;
    /// `HashAlgo::Sha256` is the audit-friendly variant. The
    /// field is independent of the wire-level cache key so an
    /// operator can keep BLAKE3 on the hot path while still
    /// asking for SHA-256 in the export sidecar.
    #[serde(default)]
    pub export: ExportConfig,
    /// B#18 / D.1.3 follow-up: embedder wiring. Default empty —
    /// `cluster_by_embedder` keeps using the dependency-free
    /// `HashingEmbedder`. Operators opt into the network-backed
    /// adapter by populating `[embedder.remote]` in
    /// `~/.config/moagan/config.toml` (or via the matching
    /// `MOAGAN_EMBEDDER_REMOTE_*` env vars applied at
    /// construction time). The section is `Option`-shaped so a
    /// missing `[embedder]` block keeps every existing run
    /// bit-identical.
    #[serde(default)]
    pub embedder: EmbedderConfig,
}

/// B#18 / D.1.3 follow-up: knobs for the optional remote embedding
/// adapter. Default empty (`None`) so the `cluster_by_embedder`
/// phase keeps using [`crate::llm::embed::HashingEmbedder`] and the
/// wire format stays dependency-free. Operators opt into the network
/// path by populating `embedder.remote` in
/// `~/.config/moagan/config.toml`.
///
/// Compliance: catalog 10-integrada-v0 §D.1.3.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedderConfig {
    /// Network-backed embedding adapter config. `None` means the
    /// hashing embedder stays in charge (the v0.7 default). The
    /// keys are NOT serialized to `manifest.json` or any audit
    /// sidecar — only the env-var *name* lives in the config;
    /// the actual key is read from `std::env::var(...)` at
    /// adapter construction time and wrapped in
    /// [`crate::secret::SecretString`].
    pub remote: Option<crate::llm::embed::RemoteEmbedderConfig>,
}

/// Export-side knobs. Mirrors `crate::cli::flags_batch::HashAlgo`
/// (the canonical CLI type) so the `--hash-algo` flag can
/// propagate without an extra conversion layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportConfig {
    /// Hash algorithm used for the export-side checksums.
    /// Default BLAKE3 — matches the internal hot-path key. The
    /// `--hash-algo blake3|sha256` flag on `moagan run` overrides
    /// this on the CLI.
    pub hash_algo: crate::cli::flags_batch::HashAlgo,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            hash_algo: crate::cli::flags_batch::HashAlgo::Blake3,
        }
    }
}

/// PR-D1: persistent per-provider temperature-profile overrides
/// for the discovery matrix. Lives under `[discovery_matrix]` in
/// `~/.config/moagan/config.toml` so an operator can pre-configure
/// a run's fan-out without passing `--temperature-profile` flags
/// every time. CLI flags win on conflict — see
/// `cli::discover::run` for the merge order.
///
/// Re-exported alongside `DiscoveryWiringConfig` (which is a
/// separate `[discovery]` block dedicated to the persona/angle
/// picker opt-ins). The two blocks are kept distinct because
/// `DiscoveryWiringConfig` is about which roles the discovery
/// pipeline auto-invokes, while this one is about how the matrix
/// fan-out iterates over temperatures and replicas.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryMatrixConfig {
    /// Per-provider sampling-temperature profiles keyed by the
    /// provider's MODEL name (the same string stored on
    /// `ProviderConfig::model`, e.g. `"MiniMax-M3"`,
    /// `"deepseek-v4-flash"`, `"mimo-v2.5"`). Empty by default so
    /// unconfigured installations match v0.5's single-shot
    /// behaviour.
    pub temperature_profiles:
        std::collections::HashMap<String, crate::discovery::matrix::TemperatureProfile>,
    /// Optional default profile applied to providers absent from
    /// [`Self::temperature_profiles`]. When `None`, the matrix
    /// uses [`crate::discovery::matrix::TemperatureProfile::default()`]
    /// (`[1.0] × 1`) — the v0.5 single-shot contract.
    pub default_profile: Option<crate::discovery::matrix::TemperatureProfile>,
    /// F1 (Track G.2): operator-supplied dimension list. Same
    /// grammar as the `--matrix-spec` CLI flag (repetible AND
    /// consolidated with `;`). When non-empty, the matrix
    /// fan-out uses these dimensions verbatim and the
    /// `discover_dimensions` phase is skipped. CLI flag wins
    /// on conflict.
    #[serde(default)]
    pub matrix_spec: Vec<String>,
    /// F1 (Track G.2): target dimension count when the operator
    /// passes `--dimensions N` without a spec. The LLM uses this
    /// as a soft hint; the actual dimension count is the
    /// `Role::DimensionDeriver`'s call. `None` lets the LLM pick
    /// freely.
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// F1 (Track G.2): target facets per dimension when the
    /// operator passes `--dimensions N --facets-per-dimension M`
    /// without a spec. `None` lets the LLM pick asymmetric
    /// counts per dimension (the F1 contract).
    #[serde(default)]
    pub facets_per_dimension: Option<usize>,
    /// F1 (Track G.2): when `true` and the operator supplied
    /// neither a `--matrix-spec` nor a persisted
    /// `[discovery_matrix].matrix_spec`, the discovery
    /// pipeline always calls `Role::DimensionDeriver` to derive
    /// the dimension list. Default `false` so existing runs
    /// that rely on the legacy `--dimensions/--facets-per-dimension`
    /// pair keep working without an LLM call. CLI
    /// `--llm-derive` flag wins on conflict.
    #[serde(default)]
    pub llm_derive_first: bool,
    /// F2 (Track G.2): sketches-per-cell floor for the matrix
    /// fan-out. The matrix's total cardinality is
    /// `cells() × sketches_per_cell`; the CLI's
    /// `--sketches-per-cell` flag (default `10`) and the
    /// `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env var override
    /// the persisted value on conflict (CLI wins, then env,
    /// then TOML). Default `10` replaces the legacy v0.5
    /// cardinality floor of `80` — the new contract decouples
    /// the per-cell fan-out from the cells count so a 4-dim
    /// × 2-facet matrix produces 80 sketches only when the
    /// operator explicitly sets `sketches_per_cell = 20`.
    /// The previous `MOAGAN_DISCOVERY_CARDINALITY` env var is
    /// removed; operators with that export must rename it.
    #[serde(default = "default_sketches_per_cell")]
    pub sketches_per_cell: usize,
}

/// F2 default: 10 sketches per cell. Replaces the v0.5
/// `cardinality = 80` floor so a fresh install fans out
/// `cells() × 10` sketches instead of always 80. Operators who
/// want the v0.5 behaviour set `sketches_per_cell = 20` (4×2)
/// or pass `--sketches-per-cell 20`.
fn default_sketches_per_cell() -> usize {
    10
}

/// F2 minimum allowed value for `sketches_per_cell`. The CLI's
/// `--sketches-per-cell` flag and the `MOAGAN_DISCOVERY_SKETCHES_PER_CELL`
/// env var reject anything below this floor. Matches the v0.5
/// spec lower band (V4 §6.4 said "40–500" sketches; F2 splits
/// that into "10 per cell minimum" + "operator picks cells count").
pub(crate) const MIN_SKETCHES_PER_CELL: usize = 10;

impl Default for DiscoveryMatrixConfig {
    fn default() -> Self {
        Self {
            temperature_profiles: std::collections::HashMap::new(),
            default_profile: None,
            matrix_spec: Vec::new(),
            dimensions: None,
            facets_per_dimension: None,
            llm_derive_first: false,
            sketches_per_cell: default_sketches_per_cell(),
        }
    }
}

/// Track E (E8 partial): knobs for the two D.7.1 catalog roles
/// that the Discovery flow can opt-into invoke. The fields here
/// gate the helpers in `src/discovery/persona_angle.rs`; the
/// `auto_pickers` knob added in v0.5 PR-18 (D.13.18) lets the
/// coordinator auto-invoke both helpers at the START of the
/// discovery loop (before the matrix fan-out) so the audit
/// sidecar can confirm `persona_picker` and `angle_picker` calls
/// precede the matrix generation calls.
///
/// All fields default to "off / no-op" (apart from
/// `auto_pickers`, which defaults to `true`) so existing runs
/// are bit-identical when the section is absent from
/// `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryWiringConfig {
    /// Whether `pick_persona` should call the LLM. When `false`
    /// (the default) the helper returns `Ok(None)` immediately
    /// and the model is never asked.
    pub persona_enabled: bool,
    /// Whether `pick_angle` should call the LLM. When `false`
    /// (the default) the helper returns `Ok(None)` immediately
    /// and the model is never asked.
    pub angle_enabled: bool,
    /// Minimum number of cluster labels the caller must supply
    /// before `pick_angle` issues an LLM call. Below this
    /// threshold the helper returns `Ok(None)` (the picker is
    /// not useful with fewer clusters than the threshold). The
    /// default `2` matches the catalog D.7.1 contract: the
    /// picker expects at least one existing angle to anchor
    /// against.
    pub angle_clusters_min: usize,
    /// D.13.9: similarity cutoff the tagger applies before it
    /// accepts a `primary` tag. Sketches whose
    /// `similarity_to_category` falls below this value are
    /// demoted to `"uncategorized"` by `tagger::sanitise`. The
    /// default matches
    /// [`crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD`]
    /// (`0.6`) so existing runs are bit-identical. Out-of-range
    /// values fall back to the default via
    /// [`crate::discovery::tagger_threshold::TaggerThreshold::from_config_value`].
    /// Set via `[discovery] tag_threshold = <0..=1>` in
    /// `~/.config/moagan/config.toml`.
    pub tag_threshold: f32,
    /// D.13.18 (v0.5 PR-18): master switch for the coordinator's
    /// auto-invocation of `run_with_pickers`. When `true` (the
    /// default), `DiscoveryCoordinator::run_with_ctx_and_target`
    /// invokes the persona picker (when `persona_enabled` is
    /// also set) and the angle picker (when `angle_enabled` is
    /// also set) at the START of the loop — before the matrix
    /// fan-out — so the audit sidecar's `calls.jsonl.gz`
    /// sidecar sees `persona_picker` and `angle_picker` rows
    /// preceding the matrix-generation rows. When `false`, the
    /// coordinator skips both helpers and the catalogue roles
    /// are not invoked unless an out-of-band caller drives
    /// them. Set via `[discovery] auto_pickers = false` in
    /// `~/.config/moagan/config.toml` or
    /// `MOAGAN_DISCOVERY_AUTO_PICKERS=false`. Existing runs that
    /// did not opt into the catalogue roles see no behavioural
    /// change because the individual `*_enabled` switches are
    /// still `false` by default.
    pub auto_pickers: bool,
}

impl Default for DiscoveryWiringConfig {
    fn default() -> Self {
        Self {
            persona_enabled: false,
            angle_enabled: false,
            angle_clusters_min: 2,
            tag_threshold: crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD,
            auto_pickers: true,
        }
    }
}

/// Track K (D9): knobs for the research fetcher grouped under
/// `research`. Currently only the bearer token; future per-host
/// rate limits or timeout overrides can land here without
/// re-shuffling the top-level `Config` layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResearchConfig {
    /// Optional bearer token for `auth_bearer`-flagged hosts. Set
    /// via `MOAGAN_RESEARCH_API_KEY=<token>` or `[research] api_key
    /// = "<token>"` in `~/.config/moagan/config.toml`. Empty /
    /// whitespace values are ignored at fetch time (see
    /// [`crate::research::ResearchFetcher::fetch_one`]). Default
    /// `None`.
    pub api_key: Option<String>,
    #[allow(missing_docs)]
    pub per_host_rate_limit: HashMap<String, RateLimitConfig>,
    /// K.4 sub-3: per-host env var name overrides for the bearer
    /// token. Keyed by canonical hostname (`docs.rs`, `crates.io`,
    /// `api.github.com`, `github.com`). Operators opt in via
    /// `[research.auth]` in `~/.config/moagan/config.toml`:
    ///
    /// ```toml
    /// [research.auth]
    /// "api.github.com" = "MY_GITHUB_TOKEN"
    /// "docs.rs" = "DOCS_RS_TOKEN"   # opt docs.rs into auth
    /// ```
    ///
    /// An empty / whitespace value falls through to the static
    /// default in
    /// [`crate::research::allowlist::HostPolicy::bearer_token_env`]
    /// so an operator can clear an entry without disabling the
    /// host's auth entirely.
    #[serde(default)]
    pub auth: ResearchAuthConfig,
}

/// K.4 sub-3: per-host env var overrides for the research
/// fetcher's bearer token. Wrapped in a newtype so the
/// `Config::research` serialisation stays flat — the inner
/// `HashMap` is the actual lookup table. Operators can also
/// override individual entries via the
/// `MOAGAN_RESEARCH_AUTH_<HOST>` env var (see
/// [`Config::apply_env_overrides`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResearchAuthConfig(
    /// Host → env var name. Keyed by canonical hostname.
    pub HashMap<String, String>,
);

impl std::ops::Deref for ResearchAuthConfig {
    type Target = HashMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ResearchAuthConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ResearchAuthConfig {
    /// Build an empty map.
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Canonicalise a hostname the same way
    /// [`crate::research::fetcher::canonical_host`] does so a
    /// TOML key of `Docs.RS` and an override lookup of `docs.rs`
    /// resolve to the same entry. The env-var helper applies
    /// the same canonicalisation so the override surface is
    /// uniform.
    pub fn insert_canonical(&mut self, host: &str, env_var: &str) -> Option<String> {
        let canonical = crate::research::fetcher::canonical_host_pub(host);
        self.0.insert(canonical, env_var.to_owned())
    }
}

fn default_startup_reconcile() -> bool {
    true
}

/// Default `SelectionPlan` for `Config::selection_plan`.
/// Spec D.21.3 baseline: `keep_top(10)`. Matches the
/// `Mode::Deep` cardinality baseline (see
/// [`crate::phases::cardinality::SelectionPlan::default_for_mode`]).
fn default_selection_plan() -> crate::phases::cardinality::SelectionPlan {
    crate::phases::cardinality::SelectionPlan::keep_top(10)
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RubricConfig {
    pub enabled: bool,
    pub validate_responses: bool,
}

impl Default for RubricConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            validate_responses: false,
        }
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub json_repair_v2_enabled: bool,
}

/// Track E (E7 partial): knobs for the Critique phase. Currently
/// only the `TiefighterCritic` adversarial cross-check switch —
/// `false` keeps the canonical `CritiquePhase` flow unchanged (the
/// N critics per proposal, no sidecar). Operators opting in flip
/// `tiefighter_enabled = true` (or set
/// `MOAGAN_CRITIQUE_TIEFIGHTER_ENABLED=true`) so the phase adds one
/// adversarial call per proposal and writes the report to
/// `<run_dir>/critiques/<proposal_id>_tiefighter.json`. Default
/// `false` keeps existing runs bit-identical.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CritiqueConfig {
    /// Whether the Critique phase should also call
    /// `Role::TiefighterCritic` on every proposal after the base
    /// critics loop and persist the resulting report as a sidecar.
    /// Default `false` (opt-in).
    pub tiefighter_enabled: bool,
}

/// Per-provider circuit breaker knobs (catalog §D.19.5).
///
/// Defaults: 5 opening errors inside a 60 s window sideline the
/// provider for 30 s. Operators can raise `threshold` for chatty
/// providers, widen `window_secs` for noisy bursts, or shrink
/// `cooldown_secs` for tight CI loops.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerConfig {
    /// Number of opening errors (per provider) inside `window_secs`
    /// that trip the breaker. Must be >= 1; `0` would mean "open
    /// after the first failure" which is closer to a hard kill than
    /// a backoff. Default 5.
    pub threshold: u32,
    /// Sliding window for the failure counter, in seconds. A
    /// failure that lands more than `window_secs` after the
    /// previous failure resets the counter. Default 60 s.
    pub window_secs: u64,
    /// Time the breaker stays open before transitioning to
    /// HalfOpen. The next call after `cooldown_secs` is the probe
    /// that decides whether to close or reopen. Default 30 s.
    pub cooldown_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            threshold: 5,
            window_secs: 60,
            cooldown_secs: 30,
        }
    }
}

/// Per-provider token-bucket knobs (catalog §D.19.6).
///
/// Each entry maps a provider name to its bucket capacity and refill
/// rate. The runtime [`crate::llm::rate_limiter::RateLimiter`] reads
/// these knobs at construction; the bucket itself lives in
/// `src/llm/rate_limiter.rs`. Defaults (60 capacity, 4 tokens/sec)
/// are a conservative profile for chatty workloads — operators
/// wanting a tighter ceiling lower `capacity` and/or raise
/// `refill_per_sec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Maximum tokens stored. Must be `>= 1`; `0` would disable the
    /// bucket entirely (every call would fail). Default 60.
    pub capacity: u32,
    /// Tokens added per second. Must be `>= 1`; `0` would never refill
    /// once the bucket is empty. Default 4.
    pub refill_per_sec: u32,
    /// Initial token count. `None` means start at full capacity
    /// (`capacity`); set lower to throttle the very first burst after
    /// boot.
    pub initial: Option<u32>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            capacity: 60,
            refill_per_sec: 4,
            initial: None,
        }
    }
}

/// v0.9.6: per-`role` adaptive throttle governor config.
/// Consumed by [`crate::llm::governor::ThrottleGovernor`]. The
/// shape is `INITIAL:MAX:INITIAL_BACKOFF_MS:MAX_BACKOFF_MS:ADDITIVE_AFTER_MS:JITTER_MS`,
/// serialised as a TOML inline table or via the matching
/// `MOAGAN_THROTTLE_PER_ROLE_<role>` env var.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ThrottleConfig {
    /// Initial per-role in-flight concurrency cap. `>= 1`. Default 4.
    pub initial_concurrency: u32,
    /// Maximum per-role in-flight concurrency cap after recovery.
    /// `>= initial_concurrency`. Default 16.
    pub max_concurrency: u32,
    /// First-429 backoff. `>= 0`; `0` means zero backoff on the
    /// first 429 (the doubling sequence then takes over from
    /// `1` ms). Default 500.
    pub initial_backoff_ms: u64,
    /// Cap on the exponential backoff sequence. Default 30_000 (30 s).
    pub max_backoff_ms: u64,
    /// Window during which a silent role (no 429s) is allowed to
    /// additively restore concurrency. Default 5_000.
    pub additive_after_ms: u64,
    /// Random jitter bound applied to the backoff sleep. Default
    /// 500.
    pub jitter_ms: u64,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            initial_concurrency: 4,
            max_concurrency: 16,
            initial_backoff_ms: 500,
            max_backoff_ms: 30_000,
            additive_after_ms: 5_000,
            jitter_ms: 500,
        }
    }
}

/// v0.9.6: per-`role` circuit breaker config. Consumed by
/// [`crate::llm::circuit_breaker::BreakerConfig`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BreakerConfig {
    /// Consecutive failures inside `window` that trip the breaker.
    /// Default 5.
    pub threshold: u32,
    /// Window during which `threshold` consecutive failures trip.
    /// Default 300.
    pub window_secs: u64,
    /// Cooldown after a trip before the half-open probe. Default 30.
    pub cooldown_secs: u64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            threshold: 5,
            window_secs: 300,
            cooldown_secs: 30,
        }
    }
}

/// Per-criterion weights used by the `RankPhase` to compute the
/// weighted score from the aggregated `JudgeScore`. Each weight is a
/// non-negative `f32`; the relative magnitude determines the
/// criterion's influence. The default is uniform (1.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RankingWeights {
    /// Weight for the average `correctness` score.
    pub correctness: f32,
    /// Weight for the average `completeness` score.
    pub completeness: f32,
    /// Weight for the average `fit` score.
    pub fit: f32,
    /// Weight for the average `evidence` score.
    pub evidence: f32,
    /// Weight for the average `clarity` score.
    pub clarity: f32,
    /// Weight for the overall `score` (the unweighted average).
    pub overall: f32,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            correctness: 1.0,
            completeness: 1.0,
            fit: 1.0,
            evidence: 1.0,
            clarity: 1.0,
            overall: 0.0,
        }
    }
}

/// Phase H (V4 §5.12 paso 6): knobs for the ranking-stability check
/// that runs after the weighted sort (step 5.6 of `RankPhase`). The
/// check perturbs `RankingWeights` with Gaussian noise and measures
/// how often each proposal keeps its position. The result is
/// persisted on `Ranking.stability_score` / `stability_label` and
/// feeds V4 §5.14's human-checkpoint trigger ("el ranking es
/// inestable").
///
/// Defaults are conservative: 8 perturbations, sigma 0.05, sensitive
/// threshold 0.8. With the default `RankingWeights` (all 1.0) and
/// sigma 0.05 the perturbations stay well inside the `[0.0, 2.0]`
/// clip range and almost never flip a clear winner. Operators wanting
/// stricter sensitivity detection raise `sigma_interactive`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StabilityConfig {
    /// Whether the stability check runs at all. `false` skips the
    /// check entirely; the ranking sidecar carries `null` for the
    /// stability fields and the human-checkpoint trigger is
    /// disarmed. Default `true`.
    pub enabled: bool,
    /// Number of weight perturbations. `0` is equivalent to
    /// `enabled = false` and short-circuits. Default 8.
    pub n_perturbations: usize,
    /// Sigma for non-interactive runs. Default 0.05.
    pub sigma_default: f32,
    /// Sigma for interactive runs (`Mode::Standard` / `Mode::Deep`
    /// without `--non-interactive`). Higher than `sigma_default`
    /// because the user is present to absorb the extra prompts when
    /// the ranking looks sensitive. Default 0.10.
    pub sigma_interactive: f32,
    /// Threshold below which the top-1 score is considered
    /// `Sensitive`. Score in `[0.0, 1.0]` = fraction of
    /// perturbations under which the winner kept its position.
    /// `score >= sensitive_threshold` => `Stable`, else
    /// `Sensitive`. Default 0.8.
    pub sensitive_threshold: f32,
    /// Deterministic seed for the perturbation RNG. Same seed =>
    /// same perturbation set => same stability score. Useful for
    /// reproducing an audit. Default 0xDEFA17_BEEF (a memorable
    /// value; change if you want run-to-run independence).
    pub seed: u64,
}

impl Default for StabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            n_perturbations: 8,
            sigma_default: 0.05,
            sigma_interactive: 0.10,
            sensitive_threshold: 0.8,
            seed: 0x00DE_FA17_BEEF_u64,
        }
    }
}

impl RankingWeights {
    /// Compute the weighted score for an aggregated judge result.
    /// The weighted score is
    /// `sum(w_i * c_i) / sum(w_i)` where `c_i` is the per-criterion
    /// average and `w_i` is the corresponding weight. The `overall`
    /// weight is added on top of the per-criterion weighted
    /// average — it lets the operator trust (or distrust) the
    /// model's `score` field above the per-criterion signal.
    pub fn weighted_score(
        &self,
        correctness: f32,
        completeness: f32,
        fit: f32,
        evidence: f32,
        clarity: f32,
        overall: f32,
    ) -> f32 {
        tracing::trace!(
            correctness,
            completeness,
            fit,
            evidence,
            clarity,
            overall,
            "RankingWeights::weighted_score: enter"
        );
        let weights = [
            self.correctness,
            self.completeness,
            self.fit,
            self.evidence,
            self.clarity,
        ];
        let criteria = [correctness, completeness, fit, evidence, clarity];
        let sum: f32 = weights.iter().sum();
        let weighted_avg = if sum > 0.0 {
            weights
                .iter()
                .zip(criteria.iter())
                .map(|(w, c)| w * c)
                .sum::<f32>()
                / sum
        } else {
            0.0
        };
        let total = sum + self.overall;
        let out = if total > 0.0 {
            (sum * weighted_avg + self.overall * overall) / total
        } else {
            0.0
        };
        tracing::trace!(
            weight_sum = sum,
            overall_weight = self.overall,
            total,
            score = out,
            "RankingWeights::weighted_score: ok"
        );
        out
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home: None,
            max_parallelism: 4,
            sketch_timeout_secs: 120,
            phase_timeout_secs: 0,
            total_timeout_secs: 0,
            token_budget: None,
            providers: default_providers(),
            default_provider: String::new(),
            export_format: "tar.gz".to_owned(),
            export_compression: 6,
            redact_in_telemetry: true,
            emit_summary: false,
            no_color: false,
            ranking_weights: RankingWeights::default(),
            rubric: RubricConfig::default(),
            llm: LlmConfig::default(),
            critique: CritiqueConfig::default(),
            repair_max_rounds: 5,
            gate_forbidden_techs: Vec::new(),
            gate_min_length: 50,
            gate_max_length: 5000,
            profile_temperature_overrides: std::collections::HashMap::new(),
            profile_judge_quorum_overrides: std::collections::HashMap::new(),
            stability: StabilityConfig::default(),
            server: ServerConfig::default(),
            retention: RetentionConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            startup_reconcile: default_startup_reconcile(),
            sandbox_allow_network: false,
            sandbox_network_policy: NetworkPolicy::default(),
            sandbox_allow_injection: false,
            sandbox_namespaces: NamespaceFlags::default(),
            sandbox_seccomp: SeccompPolicyKind::default(),
            sandbox_cgroup: None,
            research_enabled: false,
            research_urls: Vec::new(),
            research: ResearchConfig::default(),
            rate_limit_per_provider: std::collections::HashMap::new(),
            rate_limit_per_role: std::collections::HashMap::new(),
            throttle_per_role: std::collections::HashMap::new(),
            circuit_breaker_per_role: std::collections::HashMap::new(),
            discovery: DiscoveryWiringConfig::default(),
            discovery_matrix: DiscoveryMatrixConfig::default(),
            export: ExportConfig::default(),
            selection_plan: default_selection_plan(),
            embedder: EmbedderConfig::default(),
        }
    }
}

fn default_providers() -> BTreeMap<String, ProviderConfig> {
    let mut m = BTreeMap::new();

    // ----------------------------------------------------------------
    // minimax (Anthropic-compatible direct)
    // ----------------------------------------------------------------
    // One section per canonical provider family. The section name
    // (`minimax`) is the `api_keys.toml` key and the
    // `MINIMAX_API_KEY` env-var suffix — no more `kind` tag.
    // Every model the MiniMax upstream exposes lives under
    // `models[]`. The wire format (`/v1/messages`) is detected
    // from the URL by the dispatcher; the config only carries
    // the full URL.
    let minimax_endpoint = Some("https://api.minimax.io/anthropic/v1/messages".to_owned());
    let minimax_models = [
        "MiniMax-M3",
        "MiniMax-M2.7",
        "MiniMax-M2.7-highspeed",
        "MiniMax-M2.5",
    ];
    let models = minimax_models
        .iter()
        .map(|model| ModelConfig {
            id: (*model).to_owned(),
            endpoint: None,
            // Start from `DEFAULT_MAX_TOKENS` (1,000,000) and let
            // the startup auto-probe discover the real ceiling per
            // `(provider, model)`. The wire body in
            // `MinimaxProvider::send` clamps to
            // `MINIMAX_MAX_TOKENS_CAP`; `max_token_auto` below
            // enables the probe.
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        })
        .collect();
    m.insert(
        "minimax".to_owned(),
        ProviderConfig {
            models,
            endpoint: minimax_endpoint,
            temperature: Some(0.6),
            top_p: Some(0.95),
            omit_max_tokens: false,
            max_token_auto: Some(1024),
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        },
    );

    // ----------------------------------------------------------------
    // deepseek (OpenAI-compatible direct)
    // ----------------------------------------------------------------
    // v0.12.12 (§2.3): the operator's published roster
    // (`docs/proposal-03-add-ons.md` §10-integrada-v0 DeepSeek roster;
    // effective as of 2026-08-28) is `deepseek-v4-flash`,
    // `deepseek-v4-flash-vision-exp`, `deepseek-v4-pro`. The v0.12.x
    // defaults `deepseek-chat` / `deepseek-reasoner` are kept
    // registered alongside them for backward compatibility (the
    // `tests/integration_discover_deepseek.rs` --ignored suite, the
    // docs/CLI cheatsheet, and the legacy CLI fixtures still
    // reference them); the audit script and operator-facing docs
    // treat `deepseek-v4-flash` as the canonical model.
    let deepseek_endpoint = Some("https://api.deepseek.com/v1/chat/completions".to_owned());
    let deepseek_model_ids = [
        "deepseek-v4-flash",
        "deepseek-v4-flash-vision-exp",
        "deepseek-v4-pro",
        "deepseek-chat",
        "deepseek-reasoner",
    ];
    let models = deepseek_model_ids
        .iter()
        .map(|model| ModelConfig {
            id: (*model).to_owned(),
            endpoint: None,
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        })
        .collect();
    m.insert(
        "deepseek".to_owned(),
        ProviderConfig {
            models,
            endpoint: deepseek_endpoint,
            temperature: Some(0.6),
            top_p: Some(0.95),
            omit_max_tokens: false,
            max_token_auto: Some(1024),
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        },
    );

    // ----------------------------------------------------------------
    // opencode (one section, multiple models, mixed wire formats)
    // ----------------------------------------------------------------
    // The v0.10 schema exposes every OpenCode model under the
    // single `opencode` section. Each model carries its own
    // endpoint URL (the dispatcher picks the wire format from the
    // path):
    //
    // * `/v1/chat/completions` (OpenAI-compatible) — 10 models
    // * `/v1/messages` (Anthropic-compatible) — 7 models
    // * `/v1/responses` (OpenAI Responses) — 2 models
    //
    // Per-model aliases (the v0.9 single-model section shape) are
    // gone: callers reach every model via
    // `--provider opencode:MODEL`.
    let oc_chat = "https://opencode.ai/zen/go/v1/chat/completions";
    let oc_anthropic = "https://opencode.ai/zen/go/v1/messages";
    let oc_responses = "https://opencode.ai/zen/go/v1/responses";
    let oc_models = vec![
        // `/v1/chat/completions` (OpenAI-compatible).
        ModelConfig {
            id: "kimi-k3".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "kimi-k2.6".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "glm-5.1".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "glm-5.2".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        // v0.12.12 (§2.3): glm-5.3-flash — added per the operator's
        // 2026-08-28 published roster (`docs/proposal-03-add-ons.md`
        // §10-integrada-v0 OpenCode All Models). Routes over the
        // chat-completions endpoint alongside glm-5.1/glm-5.2.
        ModelConfig {
            id: "glm-5.3-flash".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "deepseek-v4-pro".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "deepseek-v4-flash".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "mimo-v2.5".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "mimo-v2.5-pro".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "hy3".to_owned(),
            endpoint: Some(oc_chat.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        // `/v1/messages` (Anthropic-compatible).
        ModelConfig {
            id: "minimax-m3".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "minimax-m2.7".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "minimax-m2.5".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "qwen3.8-max".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "qwen3.7-max".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "qwen3.7-plus".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        ModelConfig {
            id: "qwen3.6-plus".to_owned(),
            endpoint: Some(oc_anthropic.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        // `/v1/responses` (OpenAI Responses).
        ModelConfig {
            id: "gpt-5.6-luna".to_owned(),
            endpoint: Some(oc_responses.to_owned()),
            // gpt-5.6-luna rejects the *presence* of `max_tokens`,
            // not just an oversized value — the section flag
            // `omit_max_tokens = true` (applied to every model on
            // this URL via the per-model wire body) signals the
            // dispatcher to drop the field.
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
        // v0.12.12 (§2.3): muse-spark-1.2-contributor — added per
        // the operator's 2026-08-28 published roster
        // (`docs/proposal-03-add-ons.md` §10-integrada-v0 OpenCode
        // All Models). Routes over the Responses endpoint alongside
        // gpt-5.6-luna; the upstream reportedly tolerates the
        // `max_tokens` field, so we keep the default ceiling here.
        ModelConfig {
            id: "muse-spark-1.2-contributor".to_owned(),
            endpoint: Some(oc_responses.to_owned()),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
        },
    ];
    m.insert(
        "opencode".to_owned(),
        ProviderConfig {
            models: oc_models,
            endpoint: None,
            temperature: Some(1.0),
            top_p: Some(0.95),
            omit_max_tokens: false,
            max_token_auto: Some(1024),
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        },
    );

    // ----------------------------------------------------------------
    // mock
    // ----------------------------------------------------------------
    // The mock provider has no upstream; it loads canned JSON
    // fixtures via `--mock-dir`. The dispatcher still requires the
    // canonical `SECTION:MODEL` form (post-`88bcd9c`); the test
    // suite and the smoke scripts pass `--provider mock:mock-model`.
    // Registering `mock-model` here is the single source of truth
    // so the binary accepts `--provider mock:mock-model` from the
    // default config without needing a per-call MOAGAN_CONFIG
    // workaround.
    m.insert(
        "mock".to_owned(),
        ProviderConfig {
            models: vec![ModelConfig {
                id: "mock-model".to_owned(),
                endpoint: None,
                max_tokens: None,
            }],
            endpoint: Some("mock://local".to_owned()),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            // The mock provider has no upstream to probe.
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        },
    );
    m
}

/// Dashboard server knobs (T01-06 §10.8 + V4 §8.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// TCP port for `moagan telemetry view`. Default 4096
    /// (the V4 §8.8 value; the `pick_port` helper in
    /// `telemetry::dashboard` walks forward through the
    /// blacklist to land on a free port when 4096 is taken).
    pub port: u16,
    /// Bind host. Always `127.0.0.1` per V4 §8.8; exposed
    /// as a knob so operators can re-pin to `::1` if their
    /// test rig prefers IPv6 loopback.
    pub host: String,
    /// Per-request IO timeout (seconds). Default 30s.
    pub io_timeout_secs: u64,
    /// Whether the dashboard auto-creates the runs/ directory
    /// when the bind succeeds. Default true so a fresh
    /// `MOAGAN_HOME` does not require `moagan run` first.
    pub ensure_home: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 4096,
            host: "127.0.0.1".to_owned(),
            io_timeout_secs: 30,
            ensure_home: true,
        }
    }
}

/// Retention knobs (catalog D.5.1). All four fields are exposed
/// in the config so `moagan telemetry cleanup` can be tuned
/// without editing code. Set any `*_days` / `*_count` /
/// `max_storage_bytes` knob to `0` to disable that specific
/// constraint (the cleanup becomes a pure age-or-count or
/// storage pass).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Maximum age in days. Runs older than this are eligible.
    /// `0` disables the age filter.
    pub keep_runs_days: u32,
    /// Maximum total run count. `0` keeps nothing (matches
    /// the proposal's "delete all" smoke path).
    pub keep_runs_count: u32,
    /// Maximum total bytes for `.runs/`. `0` disables.
    pub max_storage_bytes: u64,
    /// Policy: `delete` (default) or `archive` (move into
    /// `<root>/archive/YYYY-MM-DD/<run_id>/`).
    pub policy: String,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            keep_runs_days: 30,
            keep_runs_count: 100,
            max_storage_bytes: 50 * 1024 * 1024 * 1024,
            policy: "delete".to_owned(),
        }
    }
}

/// Per-provider configuration.
///
/// A `[providers.<name>]` section groups every model that shares the
/// same upstream relay (MiniMax, OpenCode, DeepSeek, …). The v0.10
/// schema carries the model list as `[providers.<name>].models[]`;
/// each model carries its own id, optional endpoint override, and
/// optional `max_tokens` ceiling. The wire format (Anthropic /
/// OpenAI / OpenAI-compatible) is detected from the per-model URL
/// path, not from a kind tag on the section.
///
/// Phase 5 (CLI mandatory) updates the registry and the CLI to
/// require `--provider PROVIDER:MODEL` for every LLM-touching
/// command (`run`, `discover`, `probe`, `continue`, …). The
/// `ProviderConfig` no longer carries the deprecated `kind`,
/// `model`, or `hard_incompatibilities` singletons — the
/// dispatcher picks the wire format from `models[].endpoint` and
/// the runtime model id from `models[].id`. The section name is
/// the canonical provider-family key: it matches the
/// `api_keys.toml` `[providers]` lookup and the
/// `OPENCODE_API_KEY` / `DEEPSEEK_API_KEY` / `MINIMAX_API_KEY`
/// env-var suffix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Models this section exposes. Empty list → the section is
    /// effectively inactive (a warning, not an error — the operator
    /// may comment out a section to disable a relay temporarily).
    ///
    /// The registry iterates this list and constructs one
    /// `Provider` per `(section_name, model_id)` pair, deriving the
    /// wire format from the per-model endpoint URL.
    pub models: Vec<ModelConfig>,
    /// v0.10 (canonical): section-level default endpoint. The
    /// full URL — including the wire-format path (`/messages`,
    /// `/responses`, `/chat/completions`) — is supplied verbatim
    /// by the operator; the code never appends a path. Replaces
    /// the v0.9 `endpoint: String` singleton AND the v0.10
    /// intermediate `endpoint_new: Option<String>` so a single
    /// field carries the v0.10 schema.
    pub endpoint: Option<String>,
    /// Default sampling temperature for every model in the
    /// section. Overridden by per-role defaults in
    /// `phase.rs::resolve_temperature`. Most operators leave
    /// this at the per-role default.
    pub temperature: Option<f32>,
    /// Default nucleus sampling `top_p` for every model in the
    /// section. Overridden by per-role defaults in
    /// `phase.rs::resolve_temperature`. Most operators leave this
    /// unset and rely on the per-role default.
    pub top_p: Option<f32>,
    /// When `true`, omit the `max_tokens` field from the wire body
    /// entirely. Required for providers whose wire format rejects the
    /// *presence* of the field (e.g. OpenAI Responses when the upstream
    /// model is `gpt-5.6-luna`). Default `false` — backward-compatible
    /// with all providers that DO accept the field.
    #[serde(default)]
    pub omit_max_tokens: bool,
    /// When `Some(_)`, run the auto-probe on this provider at startup to
    /// discover the real `max_tokens` ceiling. `None` or `Some(0)`
    /// disables the probe and the config falls back to the explicit
    /// `max_tokens` knob. `Some(n)` with `n > 0` enables the probe and
    /// uses `n` as the floor for the discovered value (the probe can
    /// only raise the floor, never lower it).
    #[serde(default)]
    pub max_token_auto: Option<u32>,
    /// Explicit opt-out for the `max_tokens` auto-probe. The
    /// default (`None`) follows the registry-wide behaviour
    /// (the probe fires for every non-mock provider, with
    /// `MIN_AUTOPROBE_FLOOR` as the floor when no per-provider
    /// floor is set). Set `Some(false)` to opt a specific
    /// provider out of the probe while keeping the probe
    /// enabled globally for the rest of the registry. Set
    /// `Some(true)` to force-enable the probe even when other
    /// per-provider opt-outs would suppress it (currently a
    /// no-op for `max_token_auto`, but reserved for future
    /// matrix-level suppression switches). Operators can
    /// still use the legacy `max_token_auto = Some(0)` sentinel
    /// for the same effect — both forms are accepted as
    /// opt-out signals so existing TOML files keep working.
    #[serde(default)]
    pub max_token_auto_enabled: Option<bool>,
    /// When `true` (default), the auto-probe persists discovered values
    /// to `<MOAGAN_HOME>/max_tokens_auto.toml` so subsequent runs do not
    /// re-probe. `false` keeps the table in-memory only.
    #[serde(default = "default_max_token_auto_save")]
    pub max_token_auto_save: bool,
    /// Explicit opt-out for the `temperature` auto-probe (issue
    /// #657 fix #3). The default (`None`) follows the
    /// registry-wide behaviour (the probe fires for every
    /// non-mock provider). Set `Some(false)` to opt a specific
    /// provider out of the probe — useful for CI / smoke
    /// scripts that pre-populate
    /// `<MOAGAN_HOME>/temperatures_auto.toml` and want to skip
    /// the 21-request probe fan-out. The probe remains on by
    /// default for every other provider. Set `Some(true)` to
    /// force-enable the probe on a per-provider basis (the
    /// current default behaviour, but reserved for future
    /// matrix-level suppression switches). Operators can also
    /// use the global `MOAGAN_TEMPERATURE_AUTO` env var to
    /// flip every provider at once — CLI > env > TOML
    /// precedence, mirroring the max-tokens side.
    #[serde(default)]
    pub temperature_auto_enabled: Option<bool>,
    /// Optional token-plan declaration read by `moagan telemetry plan`.
    /// When set, the subcommand can compute a consumed-ratio against
    /// `limit_tokens` over a rolling `window_days` window derived from
    /// the `calls` table. Adding this field is purely additive: existing
    /// TOML files without a `[providers.X].plan` block continue to
    /// deserialise (the field is `Option<…>` and serde-defaults to
    /// `None`). The structured form (vs. the bare `plan_id = "weekly"`
    /// example in `docs/proposal-03-add-ons.md` §D.19.3) keeps the
    /// window length and the limit on the same struct so a CLI flag
    /// like `--window-days` can fall back to the per-provider value
    /// when the operator leaves the global default untouched.
    #[serde(default)]
    pub plan: Option<PlanConfig>,
}

/// One entry in `[providers.<name>].models[]`.
///
/// Each entry carries the model id (the value that becomes
/// `Provider::model()` at runtime), an optional endpoint override
/// (per-model URLs land on the same `models[]` list as the section
/// default), and an optional `max_tokens` ceiling. The runtime
/// dispatcher iterates the list and constructs one `Provider` per
/// entry, picking the wire format from the URL path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelConfig {
    /// Model identifier the upstream recognises (e.g.
    /// `"MiniMax-M3"`, `"kimi-k3"`). Becomes the runtime
    /// `Provider::model()` value.
    pub id: String,

    /// Optional override for the section's `endpoint`. When
    /// `None`, the model inherits the section's default. The full
    /// URL — including the wire-format path
    /// (`/messages`, `/responses`, `/chat/completions`) — is
    /// supplied verbatim by the operator; the code never
    /// appends a path.
    pub endpoint: Option<String>,

    /// Operator-supplied `max_tokens` ceiling. Becomes the
    /// per-model default; the startup auto-probe may discover a
    /// lower value (and will persist it to
    /// `<MOAGAN_HOME>/max_tokens_auto.toml`).
    pub max_tokens: Option<u32>,
}

/// Resolved spec for one `(section, model)` pair.
///
/// Built by the dispatcher at construction time (Phase 3) from the
/// per-section `ProviderConfig` plus one `ModelConfig` entry. Holds
/// the values the runtime needs without re-resolving defaults on
/// every call. The `wire_format` field is added in Phase 3 once
/// the `WireFormatId` enum lands; until then the dispatcher fills
/// it in by matching the endpoint path.
#[derive(Debug, Clone)]
pub struct ResolvedModelConfig {
    /// Section name from `[providers.<name>]` (e.g. `"minimax"`,
    /// `"opencode"`). Becomes `Provider::name()`.
    pub section: String,
    /// Model id from `models[].id`. Becomes `Provider::model()`.
    pub id: String,
    /// Fully-qualified endpoint URL (section default overridden
    /// by `models[].endpoint` when present).
    pub endpoint: String,
    /// Effective `max_tokens` (per-model, falls back to section).
    pub max_tokens: Option<u32>,
    /// Section-level default sampling temperature.
    pub temperature: Option<f32>,
    /// Section-level default top-p.
    pub top_p: Option<f32>,
    /// Wire format the dispatcher picked from the endpoint path.
    /// Computed once at construction so the runtime never has to
    /// re-parse the URL.
    pub wire_format: crate::llm::wire_format::WireFormatId,
    /// Section-level `omit_max_tokens` flag. Carried through so
    /// the per-model providers that need to drop the field from
    /// the wire body (e.g. OpenAI Responses for `gpt-5.6-luna`)
    /// can read it from the same struct.
    pub omit_max_tokens: bool,
}

/// Token-plan declaration attached to a [`ProviderConfig`]. Powers the
/// quota view in `moagan telemetry plan` and is a strict superset of
/// the `plan_id` snippet in `docs/proposal-03-add-ons.md` §D.19.3.
/// Every field is optional so a partially-filled TOML block (e.g.
/// `plan = { plan_id = "weekly" }` with no limit) still deserialises
/// without forcing the operator to spell out every knob up front.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanConfig {
    /// Human-readable plan id (e.g. `"weekly"`, `"monthly"`). Echoed
    /// verbatim in the `moagan telemetry plan` output; not used for
    /// arithmetic.
    pub plan_id: Option<String>,
    /// Hard cap on tokens consumed inside the rolling window. `None`
    /// means "no quota configured" — the subcommand still prints the
    /// observed usage but skips the consumed-ratio column.
    pub limit_tokens: Option<u64>,
    /// Length of the rolling window in days. The subcommand default
    /// is `7`; setting this on the provider lets an operator pin a
    /// monthly plan to `30` without a CLI override.
    pub window_days: Option<u32>,
}

/// Default for [`ProviderConfig::max_token_auto_save`]: persist the
/// discovered table so a subsequent run skips the probe entirely.
pub fn default_max_token_auto_save() -> bool {
    true
}

// Default impl removed — derived from struct (Default) since
// Phase 5 made all fields default-friendly.

impl ProviderConfig {
    /// First registered model's id, or empty string when the section
    /// has no models. The v0.10 dispatcher picks a concrete
    /// `(section, model_id)` pair, so callers that only know the
    /// section (e.g. the bare `--provider SECTION` shorthand) fall
    /// back to this. Pure helper — no allocation when the section
    /// has at least one model.
    pub fn first_model_id(&self) -> &str {
        let id = self.models.first().map(|m| m.id.as_str()).unwrap_or("");
        tracing::trace!(model_id = id, "ProviderConfig::first_model_id");
        id
    }
}

impl Config {
    /// Build the default configuration without touching the filesystem.
    pub fn defaults() -> Self {
        tracing::trace!("Config::defaults: building from Self::default()");
        Self::default()
    }

    /// Load configuration from the resolved config file (PR-B2
    /// strict precedence) and apply `MOAGAN_*` env overrides. Returns
    /// defaults if no file is present.
    ///
    /// Precedence (highest first):
    /// 1. `$MOAGAN_CONFIG` if set.
    /// 2. `./moagan.toml` or `./.moagan.toml` in the cwd.
    /// 3. `${XDG_CONFIG_HOME:-~/.config}/moagan/config.toml`.
    /// 4. Built-in defaults.
    ///
    /// A present cwd file (2) short-circuits the XDG lookup (3) — the
    /// two layers never merge in memory.
    ///
    /// When the user's TOML overrides the `[providers]` table, we merge
    /// it with `default_providers()`: any provider in the user's TOML
    /// replaces the default with the same name; providers absent from
    /// the user's TOML keep their built-in defaults. This way adding a
    /// new default provider (Q6 deepseek, Q7 opencode, etc.) doesn't
    /// break existing operator configs that only override a subset.
    pub fn load() -> Result<Self> {
        tracing::info!("config::load: enter");
        let path = default_config_path();
        let mut cfg = match path.as_ref() {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p)?;
                Self::warn_unknown_provider_keys(p, &raw);
                tracing::info!(
                    path = %p.display(),
                    bytes = raw.len(),
                    "config: loaded from {}",
                    p.display()
                );
                toml::from_str(&raw).map_err(|e| {
                    tracing::error!(
                        path = %p.display(),
                        error = %e,
                        "config: TOML parse failed"
                    );
                    crate::Error::InvalidArgs(format!("config parse error at {p:?}: {e}"))
                })?
            }
            _ => {
                tracing::info!("config: no file loaded; using built-in defaults");
                Self::default()
            }
        };
        // Merge user's [providers] table with the defaults: user entries win.
        let defaults = default_providers();
        let mut inserted = 0usize;
        for (name, default_spec) in defaults {
            let was_present = cfg.providers.contains_key(&name);
            cfg.providers.entry(name).or_insert(default_spec);
            if !was_present {
                inserted += 1;
            }
        }
        tracing::trace!(
            defaulted_provider_count = inserted,
            final_provider_count = cfg.providers.len(),
            "config::load: provider defaults merged"
        );
        cfg.apply_env_overrides();
        tracing::info!("config::load: ok");
        Ok(cfg)
    }

    /// Inspect the raw TOML for any `[providers.<name>]` table that
    /// only contains keys that `ProviderConfig` does NOT recognise
    /// (e.g. `api_key = "..."`). The most common offender is an
    /// operator putting the key into `moagan.toml` instead of
    /// `api_keys.toml`; without this warning the field is silently
    /// dropped by serde and the operator wonders why the key is
    /// missing. The warning fires once per offending table.
    fn warn_unknown_provider_keys(path: &std::path::Path, raw: &str) {
        tracing::trace!(path = %path.display(), "warn_unknown_provider_keys: enter");
        const KNOWN: &[&str] = &[
            "endpoint",
            "models",
            "temperature",
            "top_p",
            "omit_max_tokens",
            "max_token_auto",
            "max_token_auto_enabled",
            "max_token_auto_save",
            "temperature_auto_enabled",
            "plan",
        ];
        let parsed: toml::Value = match toml::from_str(raw) {
            Ok(v) => v,
            Err(_) => {
                tracing::trace!(
                    "warn_unknown_provider_keys: outer TOML parse failed; deferring to main loader"
                );
                return; // the main parse will surface the error.
            }
        };
        let Some(table) = parsed.get("providers").and_then(|v| v.as_table()) else {
            tracing::trace!("warn_unknown_provider_keys: no [providers] table");
            return;
        };
        let mut warned = 0usize;
        for (name, value) in table {
            let Some(sub) = value.as_table() else {
                continue;
            };
            let has_known = sub.keys().any(|k| KNOWN.contains(&k.as_str()));
            if has_known {
                continue;
            }
            let unknown: Vec<&str> = sub.keys().map(String::as_str).collect();
            tracing::warn!(
                path = %path.display(),
                provider = %name,
                unknown_keys = ?unknown,
                "config: [providers.{name}] only contains unknown keys {:?}; \
                 these are ignored (api_key belongs in api_keys.toml, not moagan.toml)",
                unknown,
            );
            warned += 1;
        }
        tracing::trace!(warned_count = warned, "warn_unknown_provider_keys: done");
    }

    /// Load a domain-specific profile by name.
    ///
    /// Thin wrapper around [`Profile::load`] so callers can stay on
    /// `Config::load_profile(...)`. The profile is NOT applied to
    /// the live `Config` here — use [`Config::apply_profile`] when
    /// the caller wants the merges to take effect (e.g. after a
    /// `--profile <name>` CLI flag resolves).
    pub fn load_profile(name: &str) -> Result<Profile> {
        tracing::debug!(name, "Config::load_profile");
        Profile::load(name)
    }

    /// Apply a profile on top of this `Config` in place.
    ///
    /// Mirrors the merge semantics of [`Profile::merge_with`]:
    /// forbidden-tech lists are unioned (deduped), child scalars
    /// win on `gate_min_length` / `gate_max_length`, and the
    /// temperature / judge-quorum override maps are recorded on
    /// the config for downstream phases that opt to read them.
    /// An empty profile is a no-op so the CLI's `--profile ""`
    /// sentinel stays harmless.
    pub fn apply_profile(&mut self, profile: &Profile) {
        tracing::debug!(
            extends = ?profile.extends,
            forbidden_count = profile.gate_forbidden_techs.len(),
            "Config::apply_profile: enter"
        );
        if profile.is_empty() {
            tracing::trace!("Config::apply_profile: empty profile; no-op");
            return;
        }
        let mut forbidden: Vec<String> = self.gate_forbidden_techs.clone();
        forbidden.extend(profile.gate_forbidden_techs.iter().cloned());
        forbidden.sort();
        forbidden.dedup();
        tracing::trace!(
            merged_count = forbidden.len(),
            "Config::apply_profile: forbidden_techs merged"
        );
        self.gate_forbidden_techs = forbidden;
        if let Some(v) = profile.gate_min_length {
            tracing::trace!(value = v, "Config::apply_profile: gate_min_length set");
            self.gate_min_length = v;
        }
        if let Some(v) = profile.gate_max_length {
            tracing::trace!(value = v, "Config::apply_profile: gate_max_length set");
            self.gate_max_length = v;
        }
        // Profile-defined temperature / judge-quorum overrides are
        // stored alongside the run config so any phase that wants
        // them can consult `Config::profile_*` without re-loading
        // the TOML. Future phases (per-role temperature wiring,
        // per-mode judge counts) read these maps directly.
        self.profile_temperature_overrides = profile.temperature_overrides.clone();
        self.profile_judge_quorum_overrides = profile.judge_quorum_overrides.clone();
        tracing::debug!(
            temp_overrides = self.profile_temperature_overrides.len(),
            quorum_overrides = self.profile_judge_quorum_overrides.len(),
            "Config::apply_profile: ok"
        );
    }

    /// Apply `MOAGAN_*` environment overrides. Any override that fails
    /// to parse is silently ignored; bad config is up to the user.
    ///
    /// `pub(crate)` so the registry tests in `llm::provider` can pin
    /// the config -> registry seam without going through the
    /// filesystem loader.
    pub(crate) fn apply_env_overrides(&mut self) {
        tracing::debug!("Config::apply_env_overrides: enter");
        if let Ok(v) = std::env::var("MOAGAN_MAX_PARALLELISM")
            && let Ok(n) = v.parse()
        {
            tracing::trace!(
                var = "MOAGAN_MAX_PARALLELISM",
                value = n,
                "applied env override"
            );
            self.max_parallelism = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_SKETCH_TIMEOUT")
            && let Ok(n) = v.parse()
        {
            tracing::trace!(
                var = "MOAGAN_SKETCH_TIMEOUT",
                value = n,
                "applied env override"
            );
            self.sketch_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_PHASE_TIMEOUT")
            && let Ok(n) = v.parse()
        {
            tracing::trace!(
                var = "MOAGAN_PHASE_TIMEOUT",
                value = n,
                "applied env override"
            );
            self.phase_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_TOTAL_TIMEOUT")
            && let Ok(n) = v.parse()
        {
            tracing::trace!(
                var = "MOAGAN_TOTAL_TIMEOUT",
                value = n,
                "applied env override"
            );
            self.total_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_DEFAULT_PROVIDER") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                tracing::trace!(var = "MOAGAN_DEFAULT_PROVIDER", value = %trimmed, "applied env override");
                self.default_provider = trimmed.to_owned();
            }
        }
        if let Ok(v) = std::env::var("MOAGAN_MINIMAX_ENDPOINT")
            && !v.trim().is_empty()
        {
            let mut rewritten = 0usize;
            for spec in self.providers.values_mut() {
                if spec
                    .endpoint
                    .as_deref()
                    .is_some_and(|e| e.contains("/messages"))
                {
                    spec.endpoint = Some(v.clone());
                    rewritten += 1;
                }
            }
            tracing::trace!(
                var = "MOAGAN_MINIMAX_ENDPOINT",
                rewritten_providers = rewritten,
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_MINIMAX_MODEL")
            && !v.trim().is_empty()
        {
            // v0.10: `model` lives on `models[].id`. Replace the
            // id on the first model of any section whose endpoint
            // matches the canonical MiniMax URL pattern (the
            // `/v1/messages` path). This mirrors the legacy
            // `MOAGAN_MINIMAX_MODEL` contract for callers that
            // still rely on the env var.
            let mut rewritten = 0usize;
            for spec in self.providers.values_mut() {
                if spec
                    .endpoint
                    .as_deref()
                    .is_some_and(|e| e.contains("/messages"))
                    && let Some(first) = spec.models.first_mut()
                {
                    first.id = v.clone();
                    rewritten += 1;
                }
            }
            tracing::trace!(
                var = "MOAGAN_MINIMAX_MODEL",
                model = %v,
                rewritten_providers = rewritten,
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_JSON_REPAIR_V2_ENABLED") {
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.llm.json_repair_v2_enabled = true,
                "false" | "0" | "no" | "off" => self.llm.json_repair_v2_enabled = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_JSON_REPAIR_V2_ENABLED",
                enabled = self.llm.json_repair_v2_enabled,
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_REPAIR_MAX_ROUNDS")
            && let Ok(n) = v.parse()
        {
            tracing::trace!(
                var = "MOAGAN_REPAIR_MAX_ROUNDS",
                value = n,
                "applied env override"
            );
            self.repair_max_rounds = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_GATE_FORBIDDEN_TECHS")
            && !v.trim().is_empty()
        {
            self.gate_forbidden_techs = v.split(',').map(|s| s.trim().to_owned()).collect();
            tracing::trace!(
                var = "MOAGAN_GATE_FORBIDDEN_TECHS",
                count = self.gate_forbidden_techs.len(),
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_STARTUP_RECONCILE") {
            // Accept the canonical `true` / `false` (case-insensitive)
            // and the bash-style `1` / `0` aliases. Anything else
            // (whitespace, garbage) leaves the existing value alone so
            // a stale export does not silently disable the boot pass.
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.startup_reconcile = true,
                "false" | "0" | "no" | "off" => self.startup_reconcile = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_STARTUP_RECONCILE",
                value = self.startup_reconcile,
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_ALLOW_NETWORK") {
            // Catalog §D.11.9. Accept the canonical `true`/`false`
            // and the bash-style `1`/`0` aliases. Stale / garbage
            // exports are ignored so a stray env var does not
            // silently flip the default.
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.sandbox_allow_network = true,
                "false" | "0" | "no" | "off" => self.sandbox_allow_network = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_SANDBOX_ALLOW_NETWORK",
                value = self.sandbox_allow_network,
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_ALLOW_INJECTION") {
            // Catalog §D.11.10. Same parsing as the network flag;
            // missing / garbage values leave the existing knob alone.
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.sandbox_allow_injection = true,
                "false" | "0" | "no" | "off" => self.sandbox_allow_injection = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_SANDBOX_ALLOW_INJECTION",
                value = self.sandbox_allow_injection,
                "applied env override"
            );
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_NETWORK_POLICY")
            && let Some(policy) = parse_network_policy_env(&v)
        {
            tracing::trace!(
                var = "MOAGAN_SANDBOX_NETWORK_POLICY",
                policy = ?policy,
                "applied env override"
            );
            self.sandbox_network_policy = policy;
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_NAMESPACES")
            && let Ok(flags) = v.parse()
        {
            tracing::trace!(
                var = "MOAGAN_SANDBOX_NAMESPACES",
                flags = ?flags,
                "applied env override"
            );
            self.sandbox_namespaces = flags;
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_SECCOMP")
            && let Some(kind) = parse_seccomp_policy_env(&v)
        {
            tracing::trace!(
                var = "MOAGAN_SANDBOX_SECCOMP",
                kind = ?kind,
                "applied env override"
            );
            self.sandbox_seccomp = kind;
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_CGROUP") {
            // Catalog §D.11.1. The env var accepts:
            // - `enabled` / `1` / `true` / `on` → opt in with the
            //   canonical default profile.
            // - a JSON object with `cpu_max` / `memory_max_bytes` /
            //   `pids_max` → opt in with a custom profile.
            // - anything else (including empty / whitespace) → leave
            //   the existing knob alone so a stale export does not
            //   silently flip the default.
            let normalised = v.trim();
            if matches!(
                normalised.to_ascii_lowercase().as_str(),
                "enabled" | "1" | "true" | "yes" | "on"
            ) {
                self.sandbox_cgroup = Some(CgroupLimits::default());
            } else if let Some(limits) = parse_cgroup_limits_env(normalised) {
                self.sandbox_cgroup = Some(limits);
            }
            tracing::trace!(
                var = "MOAGAN_SANDBOX_CGROUP",
                is_some = self.sandbox_cgroup.is_some(),
                "applied env override"
            );
        }
        // Track K (D9): bound the external research fetcher to an
        // explicit opt-in. `MOAGAN_RESEARCH_ENABLED=true` flips the
        // flag; everything else leaves the config alone so a stale
        // `false` / blank export does not silently toggle the
        // feature.
        if let Ok(v) = std::env::var("MOAGAN_RESEARCH_ENABLED") {
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.research_enabled = true,
                "false" | "0" | "no" | "off" => self.research_enabled = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_RESEARCH_ENABLED",
                value = self.research_enabled,
                "applied env override"
            );
        }
        // Track E (E7 partial): opt-in switch for the
        // `TiefighterCritic` adversarial cross-check sidecar.
        // `MOAGAN_CRITIQUE_TIEFIGHTER_ENABLED=true` flips the flag;
        // anything else (including garbage / blank) leaves the
        // existing knob alone so a stale export does not silently
        // re-enable the sidecar.
        if let Ok(v) = std::env::var("MOAGAN_CRITIQUE_TIEFIGHTER_ENABLED") {
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.critique.tiefighter_enabled = true,
                "false" | "0" | "no" | "off" => self.critique.tiefighter_enabled = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_CRITIQUE_TIEFIGHTER_ENABLED",
                value = self.critique.tiefighter_enabled,
                "applied env override"
            );
        }
        // Track K (D9): CSV list of URLs the bounded fetcher is
        // allowed to query. Empty / whitespace exports are ignored
        // so a stale empty value cannot mask a user-supplied URL
        // list. The host-allowlist filter is enforced at fetch
        // time inside `research::fetch_all`, not here, so the
        // env-var path stays a pure ownership-of-config.
        if let Ok(v) = std::env::var("MOAGAN_RESEARCH_URLS")
            && !v.trim().is_empty()
        {
            self.research_urls = v
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            tracing::trace!(
                var = "MOAGAN_RESEARCH_URLS",
                count = self.research_urls.len(),
                "applied env override"
            );
        }
        // Track K (D9): bearer-token wiring for `auth_bearer`-flagged
        // hosts (`api.github.com` in the canonical allowlist). Empty
        // / whitespace exports are kept-as-`None` so a stale shell
        // export cannot forge an Authorization header with an empty
        // value. Mirrors the `MOAGAN_RESEARCH_URLS` handling: the
        // env var only owns the config surface, the actual wiring
        // happens at fetch time inside
        // `ResearchFetcher::fetch_one`.
        if let Ok(v) = std::env::var("MOAGAN_RESEARCH_API_KEY")
            && !v.trim().is_empty()
        {
            self.research.api_key = Some(v);
            tracing::trace!(var = "MOAGAN_RESEARCH_API_KEY", "applied env override");
        }
        // K.4 sub-3: per-host env var overrides. Operators can
        // set `MOAGAN_RESEARCH_AUTH_<HOST>=<env_var_name>` to
        // redirect the bearer-token lookup for a specific host
        // without editing `~/.config/moagan/config.toml`. The
        // suffix is uppercased and the host characters (`.`,
        // `-`) are rewritten to `_` so `api.github.com` maps to
        // `MOAGAN_RESEARCH_AUTH_API_GITHUB_COM`. Garbage
        // suffixes (no recognisable host fragment) are silently
        // dropped so a stale export does not corrupt the
        // override map.
        let mut auth_overrides = 0usize;
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_RESEARCH_AUTH_") else {
                continue;
            };
            let trimmed_value = value.trim();
            if trimmed_value.is_empty() {
                continue;
            }
            // Decode the env-var suffix back to a hostname.
            // The mapping is bijective: every `_` in the
            // suffix becomes `.` and the case is lowercased.
            // The `.` ↔ `_` rewrite matches the operator's
            // mental model and is the inverse of what we do
            // when canonicalising a hostname for the lookup.
            let host = suffix.to_ascii_lowercase().replace('_', ".");
            if host.is_empty() {
                continue;
            }
            // Re-canonicalise through the fetcher helper so
            // the lookup keys line up with what
            // `bearer_token_env_for` resolves at fetch time.
            let canonical = crate::research::fetcher::canonical_host_pub(&host);
            if canonical.is_empty() {
                continue;
            }
            self.research
                .auth
                .0
                .insert(canonical, trimmed_value.to_owned());
            auth_overrides += 1;
        }
        if auth_overrides > 0 {
            tracing::trace!(
                prefix = "MOAGAN_RESEARCH_AUTH_",
                count = auth_overrides,
                "applied env override"
            );
        }
        let mut rate_limit_overrides = 0usize;
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_RESEARCH_RATE_LIMIT_") else {
                continue;
            };
            let host = canonical_research_rate_limit_host(suffix);
            if host.is_empty() {
                continue;
            }
            if let Some(config) = parse_rate_limit_env(&value) {
                self.research.per_host_rate_limit.insert(host, config);
                rate_limit_overrides += 1;
            }
        }
        if rate_limit_overrides > 0 {
            tracing::trace!(
                prefix = "MOAGAN_RESEARCH_RATE_LIMIT_",
                count = rate_limit_overrides,
                "applied env override"
            );
        }
        // D.13.18 (v0.5 PR-18): master switch for the coordinator's
        // auto-invocation of `run_with_pickers`. The env-var name
        // matches the TOML key (`auto_pickers`) so operators can
        // flip the bit either way without touching the config file.
        // Garbage / blank exports leave the existing value alone so
        // a stale export does not silently toggle the helper.
        // F2 (Track G.2): the discovery `sketches_per_cell` knob
        // replaces the legacy v0.5 `cardinality` floor. The env var
        // `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` overrides the TOML
        // value on conflict; the CLI flag `--sketches-per-cell` wins
        // over both (see `cli::discover::run`). Garbage / blank
        // exports leave the existing value alone so a stale env
        // var does not silently flip the floor.
        if let Ok(v) = std::env::var("MOAGAN_DISCOVERY_SKETCHES_PER_CELL")
            && let Ok(n) = v.trim().parse::<usize>()
            && n >= MIN_SKETCHES_PER_CELL
        {
            tracing::trace!(
                var = "MOAGAN_DISCOVERY_SKETCHES_PER_CELL",
                value = n,
                "applied env override"
            );
            self.discovery_matrix.sketches_per_cell = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_DISCOVERY_AUTO_PICKERS") {
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.discovery.auto_pickers = true,
                "false" | "0" | "no" | "off" => self.discovery.auto_pickers = false,
                _ => {}
            }
            tracing::trace!(
                var = "MOAGAN_DISCOVERY_AUTO_PICKERS",
                value = self.discovery.auto_pickers,
                "applied env override"
            );
        }
        // Track E (catalog §D.19.6): per-provider rate-limit knobs.
        // `MOAGAN_RATE_LIMIT_<provider>=<capacity>:<refill_per_sec>`
        // opts the named provider into the token bucket. Each entry
        // overwrites any previous value for the same provider so the
        // env var is the canonical last-write-wins surface. Garbage
        // values (missing colon, non-numeric tokens) are silently
        // ignored so a stale export does not corrupt an existing
        // TOML-loaded entry. The provider name is lowercased to
        // match the canonical `[providers]` table keys. Roles are
        // a separate prefix (`MOAGAN_RATE_LIMIT_ROLE_<role>`) and
        // are skipped here so a per-role env var is not misread as
        // a provider named `role.tagger`.
        let mut rl_provider = 0usize;
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_RATE_LIMIT_") else {
                continue;
            };
            if suffix.is_empty() || suffix.starts_with("ROLE_") {
                continue;
            }
            let provider = suffix.to_ascii_lowercase();
            if let Some(cfg) = parse_rate_limit_env(&value) {
                self.rate_limit_per_provider.insert(provider, cfg);
                rl_provider += 1;
            }
        }
        if rl_provider > 0 {
            tracing::trace!(
                prefix = "MOAGAN_RATE_LIMIT_",
                count = rl_provider,
                final_size = self.rate_limit_per_provider.len(),
                "applied env override"
            );
        }
        // Track E (catalog §D.19.6): per-role rate-limit knobs.
        // `MOAGAN_RATE_LIMIT_ROLE_<role>=<capacity>:<refill_per_sec>`
        // opts the named role into a role-scoped token bucket that
        // is acquired by `call_with_retry` / `call_uncached` on
        // top of the per-provider bucket. Same last-write-wins
        // semantics as the per-provider env var; the role name
        // matches the `Role::as_str()` value (snake_case). Garbage
        // values (missing colon, non-numeric tokens) are silently
        // ignored so a stale export does not corrupt an existing
        // TOML-loaded entry.
        let mut rl_role = 0usize;
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_RATE_LIMIT_ROLE_") else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            let role = suffix.to_ascii_lowercase();
            if let Some(cfg) = parse_rate_limit_env(&value) {
                self.rate_limit_per_role.insert(role, cfg);
                rl_role += 1;
            }
        }
        if rl_role > 0 {
            tracing::trace!(
                prefix = "MOAGAN_RATE_LIMIT_ROLE_",
                count = rl_role,
                final_size = self.rate_limit_per_role.len(),
                "applied env override"
            );
        }
        // v0.9.6: per-role adaptive-throttle governor knobs.
        // `MOAGAN_THROTTLE_PER_ROLE_<role>=<initial>:<max>:<init_backoff>:<max_backoff>:<additive_after>:<jitter>`
        // opts the named role into the AIMD governor. The role
        // name matches `Role::as_str()` value (snake_case).
        // Garbage values (missing colons, non-numeric tokens) are
        // silently ignored so a stale export does not corrupt an
        // existing TOML-loaded entry.
        let mut throttle = 0usize;
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_THROTTLE_PER_ROLE_") else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            let role = suffix.to_ascii_lowercase();
            if let Some(cfg) = parse_throttle_env(&value) {
                self.throttle_per_role.insert(role, cfg);
                throttle += 1;
            }
        }
        if throttle > 0 {
            tracing::trace!(
                prefix = "MOAGAN_THROTTLE_PER_ROLE_",
                count = throttle,
                final_size = self.throttle_per_role.len(),
                "applied env override"
            );
        }
        // v0.9.6: per-role circuit-breaker knobs.
        // `MOAGAN_CIRCUIT_BREAKER_PER_ROLE_<role>=<threshold>:<window_secs>:<cooldown_secs>`
        // opts the named role into a per-(provider, role) breaker
        // that trips on persistent `PlanExhausted`. The role name
        // matches `Role::as_str()` value (snake_case). Garbage
        // values are silently ignored.
        let mut cb = 0usize;
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_CIRCUIT_BREAKER_PER_ROLE_") else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            let role = suffix.to_ascii_lowercase();
            if let Some(cfg) = parse_breaker_env(&value) {
                self.circuit_breaker_per_role.insert(role, cfg);
                cb += 1;
            }
        }
        if cb > 0 {
            tracing::trace!(
                prefix = "MOAGAN_CIRCUIT_BREAKER_PER_ROLE_",
                count = cb,
                final_size = self.circuit_breaker_per_role.len(),
                "applied env override"
            );
        }
        // Per-provider `omit_max_tokens` override from env vars of the
        // form `MOAGAN_<NAME>_OMIT_MAX_TOKENS=true|false`. The provider
        // name is uppercased and both dots and hyphens are rewritten to
        // underscores so `gpt-5.6-luna` becomes
        // `MOAGAN_OPENCODE_OMIT_MAX_TOKENS`. Garbage values are
        // silently ignored so a stale export does not silently flip
        // the flag.
        //
        // The two auto-probe knobs below are read once and applied to
        // every provider: they are a global kill-switch / floor rather
        // than a per-provider name-mangled variable, so an operator can
        // disable the probe fleet-wide with a single export.
        let auto_env = std::env::var("MOAGAN_MAX_TOKEN_AUTO").ok();
        let auto_save_env = std::env::var("MOAGAN_MAX_TOKEN_AUTO_SAVE").ok();
        // Issue #657 fix #3: operator-facing kill-switch for the
        // temperature auto-probe (mirror of `MOAGAN_MAX_TOKEN_AUTO`).
        // The default behaviour keeps the probe on for every
        // non-mock provider; an exported `false` (or any off-spelling
        // — `0` / `no` / `off`) flips every provider to
        // `temperature_auto_enabled = Some(false)` so the
        // background probe fan-out is skipped. Truthy / unrecognised
        // values are silently ignored so a typo does not silently
        // disable the probe — same convention as
        // `MOAGAN_<name>_OMIT_MAX_TOKENS` above.
        let temp_auto_env = std::env::var("MOAGAN_TEMPERATURE_AUTO").ok();
        if auto_env.is_some() || auto_save_env.is_some() || temp_auto_env.is_some() {
            tracing::trace!(
                has_max_token_auto = auto_env.is_some(),
                has_max_token_auto_save = auto_save_env.is_some(),
                has_temperature_auto = temp_auto_env.is_some(),
                provider_count = self.providers.len(),
                "applying global max_token_auto / temperature_auto knobs"
            );
        }
        for (name, spec) in self.providers.iter_mut() {
            let env_key = format!(
                "MOAGAN_{}_OMIT_MAX_TOKENS",
                name.to_uppercase().replace(['.', '-'], "_")
            );
            if let Ok(v) = std::env::var(&env_key) {
                match v.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => spec.omit_max_tokens = true,
                    "false" | "0" | "no" | "off" | "" => spec.omit_max_tokens = false,
                    _ => {}
                }
            }
            // `Some(0)` is the "off" sentinel: `probe_table::from_path`
            // clamps the floor to `MIN_AUTOPROBE_FLOOR`, and the
            // registry treats `Some(0)` exactly like `None` when
            // deciding whether to attach the table.
            if let Some(v) = auto_env.as_deref() {
                spec.max_token_auto = match v.trim() {
                    "" | "off" | "false" | "0" => Some(0), // off (alias for None)
                    s => s.parse::<u32>().ok(),
                };
            }
            if let Some(v) = auto_save_env.as_deref() {
                spec.max_token_auto_save = matches!(v.trim(), "true" | "1" | "yes" | "on" | "");
            }
            // Issue #657 fix #3: `MOAGAN_TEMPERATURE_AUTO=false`
            // sets `temperature_auto_enabled = Some(false)` on every
            // provider so the temperature probe is skipped entirely.
            // Truthy / unrecognised values leave the field alone so
            // the registry-side default ("probe runs") is preserved.
            // The "off" set is intentionally the same as
            // `MOAGAN_<name>_OMIT_MAX_TOKENS` so the operator
            // mental model stays consistent across env vars.
            if let Some(v) = temp_auto_env.as_deref() {
                match v.trim().to_ascii_lowercase().as_str() {
                    "false" | "0" | "no" | "off" => spec.temperature_auto_enabled = Some(false),
                    "true" | "1" | "yes" | "on" => spec.temperature_auto_enabled = Some(true),
                    _ => {}
                }
            }
        }
        tracing::trace!(
            provider_count = self.providers.len(),
            rl_provider_entries = self.rate_limit_per_provider.len(),
            rl_role_entries = self.rate_limit_per_role.len(),
            throttle_entries = self.throttle_per_role.len(),
            cb_entries = self.circuit_breaker_per_role.len(),
            "Config::apply_env_overrides: ok"
        );
    }
}

/// Parse the `MOAGAN_SANDBOX_CGROUP` env var (when it does not look
/// like a truthy flag) into a [`CgroupLimits`] profile. Returns
/// `None` for any value that does not parse; the caller is
/// expected to leave the existing knob alone in that case.
fn parse_cgroup_limits_env(s: &str) -> Option<CgroupLimits> {
    tracing::trace!(value_len = s.len(), "parse_cgroup_limits_env: enter");
    match serde_json::from_str::<CgroupLimits>(s) {
        Ok(limits) => {
            tracing::trace!("parse_cgroup_limits_env: ok");
            Some(limits)
        }
        Err(_) => {
            tracing::trace!("parse_cgroup_limits_env: parse failed; returning None");
            None
        }
    }
}

fn canonical_research_rate_limit_host(suffix: &str) -> String {
    let host = suffix
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
        .replace('_', ".");
    tracing::trace!(suffix, resolved = %host, "canonical_research_rate_limit_host");
    host
}

/// Parse the `MOAGAN_RATE_LIMIT_<provider>` env var into a
/// [`RateLimitConfig`]. Accepts `<capacity>:<refill_per_sec>` (both
/// non-negative integers). Returns `None` for any value that does
/// not parse so a stale / malformed export leaves the existing knob
/// alone.
fn parse_rate_limit_env(s: &str) -> Option<RateLimitConfig> {
    tracing::trace!(value = s, "parse_rate_limit_env: enter");
    let s = s.trim();
    let (cap_str, refill_str) = s.split_once(':')?;
    let capacity: u32 = cap_str.trim().parse().ok()?;
    let refill_per_sec: u32 = refill_str.trim().parse().ok()?;
    let cfg = RateLimitConfig {
        capacity,
        refill_per_sec,
        initial: None,
    };
    tracing::trace!(capacity, refill_per_sec, "parse_rate_limit_env: ok");
    Some(cfg)
}

/// Parse the `MOAGAN_THROTTLE_PER_ROLE_<role>` env var. The shape is
/// `INITIAL:MAX:INITIAL_BACKOFF_MS:MAX_BACKOFF_MS:ADDITIVE_AFTER_MS:JITTER_MS`.
/// Returns `None` for any value that does not parse so a stale /
/// malformed export leaves the existing knob alone.
fn parse_throttle_env(s: &str) -> Option<ThrottleConfig> {
    tracing::trace!(value = s, "parse_throttle_env: enter");
    let mut tokens = s.trim().split(':').map(str::trim);
    let initial_concurrency: u32 = tokens.next()?.parse().ok()?;
    let max_concurrency: u32 = tokens.next()?.parse().ok()?;
    let initial_backoff_ms: u64 = tokens.next()?.parse().ok()?;
    let max_backoff_ms: u64 = tokens.next()?.parse().ok()?;
    let additive_after_ms: u64 = tokens.next()?.parse().ok()?;
    let jitter_ms: u64 = tokens.next()?.parse().ok()?;
    let cfg = ThrottleConfig {
        initial_concurrency,
        max_concurrency,
        initial_backoff_ms,
        max_backoff_ms,
        additive_after_ms,
        jitter_ms,
    };
    tracing::trace!(
        initial_concurrency,
        max_concurrency,
        "parse_throttle_env: ok"
    );
    Some(cfg)
}

/// Parse the `MOAGAN_CIRCUIT_BREAKER_PER_ROLE_<role>` env var.
/// The shape is `THRESHOLD:WINDOW_SECS:COOLDOWN_SECS`. Returns
/// `None` for any value that does not parse.
fn parse_breaker_env(s: &str) -> Option<BreakerConfig> {
    tracing::trace!(value = s, "parse_breaker_env: enter");
    let mut tokens = s.trim().split(':').map(str::trim);
    let threshold: u32 = tokens.next()?.parse().ok()?;
    let window_secs: u64 = tokens.next()?.parse().ok()?;
    let cooldown_secs: u64 = tokens.next()?.parse().ok()?;
    let cfg = BreakerConfig {
        threshold,
        window_secs,
        cooldown_secs,
    };
    tracing::trace!(
        threshold,
        window_secs,
        cooldown_secs,
        "parse_breaker_env: ok"
    );
    Some(cfg)
}

/// Parse the `MOAGAN_SANDBOX_SECCOMP` env var into a
/// [`SeccompPolicyKind`].
///
/// Accepts:
/// - `permissive` / `PERMISSIVE` — [`SeccompPolicyKind::Permissive`]
/// - `strict_rust_build` / `STRICT_RUST_BUILD` —
///   [`SeccompPolicyKind::StrictRustBuild`]
/// - the JSON string form (`"permissive"` / `"strict_rust_build"`)
///
/// Returns `None` for any value that does not parse; the caller is
/// expected to leave the existing knob alone in that case so a
/// stale / malformed export does not silently flip the install.
fn parse_seccomp_policy_env(s: &str) -> Option<SeccompPolicyKind> {
    tracing::trace!(value = s, "parse_seccomp_policy_env: enter");
    let s = s.trim();
    if s.eq_ignore_ascii_case("permissive") {
        tracing::trace!("parse_seccomp_policy_env: permissive (case-insensitive)");
        return Some(SeccompPolicyKind::Permissive);
    }
    if s.eq_ignore_ascii_case("strict_rust_build") {
        tracing::trace!("parse_seccomp_policy_env: strict_rust_build (case-insensitive)");
        return Some(SeccompPolicyKind::StrictRustBuild);
    }
    if let Ok(kind) = serde_json::from_str::<SeccompPolicyKind>(s) {
        tracing::trace!(kind = ?kind, "parse_seccomp_policy_env: JSON parse");
        return Some(kind);
    }
    tracing::trace!("parse_seccomp_policy_env: no match; returning None");
    None
}

/// Parse the `MOAGAN_SANDBOX_NETWORK_POLICY` env var into a
/// [`NetworkPolicy`].
///
/// Accepts:
/// - `off` / `OFF` — [`NetworkPolicy::Off`]
/// - `open` / `OPEN` — [`NetworkPolicy::Open`]
/// - `["host1","host2",...]` (JSON array) —
///   [`NetworkPolicy::AllowList`]
/// - the full NetworkPolicy JSON form
///   (`{"kind":"allow_list","hosts":["a","b"]}`)
/// - `allow_list` alone — empty `AllowList` (semantically
///   equivalent to `Off`).
///
/// Returns `None` for any value that does not parse; the caller is
/// expected to leave the existing knob alone in that case so a
/// stale export does not silently corrupt the install.
fn parse_network_policy_env(s: &str) -> Option<NetworkPolicy> {
    tracing::trace!(value = s, "parse_network_policy_env: enter");
    let s = s.trim();
    if s.eq_ignore_ascii_case("off") {
        tracing::trace!("parse_network_policy_env: off");
        return Some(NetworkPolicy::Off);
    }
    if s.eq_ignore_ascii_case("open") {
        tracing::trace!("parse_network_policy_env: open");
        return Some(NetworkPolicy::Open);
    }
    if let Ok(list) = serde_json::from_str::<Vec<String>>(s) {
        tracing::trace!(
            host_count = list.len(),
            "parse_network_policy_env: JSON array -> AllowList"
        );
        return Some(NetworkPolicy::AllowList { hosts: list });
    }
    if let Ok(policy) = serde_json::from_str::<NetworkPolicy>(s) {
        tracing::trace!(policy = ?policy, "parse_network_policy_env: typed JSON");
        return Some(policy);
    }
    if s.eq_ignore_ascii_case("allow_list") {
        tracing::trace!("parse_network_policy_env: bare 'allow_list' (empty)");
        return Some(NetworkPolicy::AllowList { hosts: Vec::new() });
    }
    tracing::trace!("parse_network_policy_env: no match; returning None");
    None
}

impl Config {
    /// Resolve the configured provider by name. Returns
    /// `Error::InvalidArgs` if the provider is unknown.
    pub fn provider(&self, name: &str) -> Result<&ProviderConfig> {
        tracing::trace!(name, "Config::provider: enter");
        let res = self.providers.get(name).ok_or_else(|| {
            tracing::warn!(name, "Config::provider: unknown provider");
            crate::Error::InvalidArgs(format!("unknown provider: {name}"))
        });
        if let Ok(spec) = &res {
            tracing::trace!(
                name,
                model_count = spec.models.len(),
                "Config::provider: ok"
            );
        }
        res
    }

    /// Resolve the operator-supplied `--provider` value (or its
    /// default-source equivalent) into a `(section, model_id)` pair.
    ///
    /// `raw` accepts two shapes:
    ///
    /// * `SECTION` (e.g. `"minimax"`) — the section name itself.
    ///   When the section exposes exactly one model, that model is
    ///   picked; otherwise the helper errors with a clear message
    ///   asking the operator to disambiguate via `SECTION:MODEL`.
    /// * `SECTION:MODEL` (e.g. `"opencode:kimi-k3"`) — both halves
    ///   are explicit.
    ///
    /// Empty halves and extra colons are rejected with
    /// `Error::InvalidArgs` so the CLI surface stays unambiguous.
    pub fn resolve_provider(&self, raw: &str) -> Result<(String, String)> {
        tracing::trace!(raw, "Config::resolve_provider: enter");
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            tracing::warn!("Config::resolve_provider: empty input");
            return Err(crate::Error::InvalidArgs(
                "--provider is empty; pass SECTION[:MODEL]".into(),
            ));
        }
        if let Some((section, model)) = trimmed.split_once(':') {
            if section.is_empty() || model.is_empty() {
                tracing::warn!(raw, "Config::resolve_provider: empty half");
                return Err(crate::Error::InvalidArgs(format!(
                    "--provider '{trimmed}' has an empty section or model half"
                )));
            }
            if model.contains(':') {
                tracing::warn!(raw, "Config::resolve_provider: too many ':'");
                return Err(crate::Error::InvalidArgs(format!(
                    "--provider '{trimmed}' has more than one ':'; expected exactly one separator"
                )));
            }
            tracing::debug!(
                section,
                model,
                "Config::resolve_provider: SECTION:MODEL form"
            );
            return Ok((section.to_owned(), model.to_owned()));
        }
        // Bare SECTION: pick the section's first model (single-model
        // alias sections like `minimax` / `deepseek` expose exactly
        // one; multi-model sections like `opencode` reject the bare
        // form so the operator picks a model explicitly).
        let spec = self.provider(trimmed)?;
        if spec.models.is_empty() {
            tracing::warn!(
                section = trimmed,
                "Config::resolve_provider: bare section has no models"
            );
            return Err(crate::Error::InvalidArgs(format!(
                "--provider '{trimmed}' has no model configured; pass --provider {trimmed}:MODEL explicitly"
            )));
        }
        if spec.models.len() > 1 {
            tracing::warn!(
                section = trimmed,
                model_count = spec.models.len(),
                "Config::resolve_provider: bare section ambiguous"
            );
            return Err(crate::Error::InvalidArgs(format!(
                "--provider '{trimmed}' exposes {} models; pass --provider {trimmed}:MODEL explicitly (one of: {})",
                spec.models.len(),
                spec.models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        tracing::debug!(
            section = trimmed,
            model = %spec.models[0].id,
            "Config::resolve_provider: bare section -> first model"
        );
        Ok((trimmed.to_owned(), spec.models[0].id.clone()))
    }

    /// Resolve the configured `(section, model)` pair into a
    /// [`ResolvedModelConfig`] ready for the dispatcher. Looks up
    /// `self.providers[section]` and then `models[]` for the
    /// requested model id. The per-model `endpoint` override (when
    /// present) wins over the section-level default.
    ///
    /// Returns `Error::InvalidArgs` when the section is missing or
    /// the section exists but the model id is not registered under
    /// it. The wire format is derived from the resolved endpoint
    /// URL via [`crate::llm::wire_format::wire_format_from_url`]
    /// so the dispatcher does not have to recompute it.
    pub fn resolved_model(&self, section: &str, model_id: &str) -> Result<ResolvedModelConfig> {
        tracing::trace!(section, model_id, "Config::resolved_model: enter");
        let spec = self.provider(section)?;
        let model_cfg = spec
            .models
            .iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| {
                tracing::warn!(section, model_id, "Config::resolved_model: unknown model");
                crate::Error::InvalidArgs(format!(
                    "provider '{section}' has no model '{model_id}'; \
                     registered models: [{}]",
                    spec.models
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
        let endpoint = model_cfg
            .endpoint
            .clone()
            .or_else(|| spec.endpoint.clone())
            .ok_or_else(|| {
                tracing::error!(section, model_id, "Config::resolved_model: no endpoint");
                crate::Error::InvalidArgs(format!(
                    "provider '{section}' model '{model_id}' has no endpoint \
                     configured (neither section nor model specifies one)"
                ))
            })?;
        let wire_format =
            crate::llm::wire_format::wire_format_from_url(&endpoint).map_err(|e| {
                tracing::error!(
                    section,
                    model_id,
                    endpoint = %endpoint,
                    error = %e,
                    "Config::resolved_model: wire_format_from_url failed"
                );
                crate::Error::InvalidArgs(format!("provider '{section}' model '{model_id}': {e}"))
            })?;
        let resolved = ResolvedModelConfig {
            section: section.to_owned(),
            id: model_id.to_owned(),
            endpoint,
            max_tokens: model_cfg.max_tokens,
            temperature: spec.temperature,
            top_p: spec.top_p,
            wire_format,
            omit_max_tokens: spec.omit_max_tokens,
        };
        tracing::debug!(
            section,
            model_id,
            wire_format = ?resolved.wire_format,
            "Config::resolved_model: ok"
        );
        Ok(resolved)
    }

    /// Whether `Role::JsonRepairV2` should fire on parse failure for
    /// this run mode. The decision is consumed at the single
    /// `call_with_retry_parse` gate in `phases::phase`.
    ///
    /// Default-on for `discover` (`moagan discover` spawns 40-500
    /// sketches; the extra LLM call cost amortises across the
    /// matrix fan-out, and the stubborn-model failure modes that
    /// `JsonRepairV2` catches — truncated brackets inside a
    /// think-block, prose-prefixed JSON the tolerant extractor
    /// cannot splice — are exactly the modes stubborn models
    /// exhibit at the sketch-fan-out scale). Every other mode
    /// respects the explicit `llm.json_repair_v2_enabled` config
    /// flag (default `false`) so the token-cost surprise stays
    /// opt-in.
    ///
    /// `mode` is the same lowercase string the run dispatcher
    /// stores on [`crate::phases::RunContext::mode`] (e.g.
    /// `"discover"` for `moagan discover`). The match is
    /// exhaustive via the catch-all: an unknown mode string
    /// (corrupted manifest, future mode added without updating
    /// this helper) falls back to the explicit config flag,
    /// which preserves today's behaviour.
    pub fn json_repair_v2_enabled_for_mode(&self, mode: &str) -> bool {
        let enabled = if mode == "discover" {
            true
        } else {
            self.llm.json_repair_v2_enabled
        };
        tracing::trace!(
            mode,
            explicit_flag = self.llm.json_repair_v2_enabled,
            enabled,
            "Config::json_repair_v2_enabled_for_mode"
        );
        enabled
    }
}

/// Resolve the config-file path. Precedence (PR-B2, strict — no
/// merge between layers):
///
/// 1. `$MOAGAN_CONFIG` if set (verbatim, unchanged).
/// 2. `./moagan.toml`  (cwd primary file name).
/// 3. `./.moagan.toml` (cwd hidden alt name).
/// 4. `${XDG_CONFIG_HOME:-~/.config}/moagan/config.toml`
///    (user-level XDG fallback, only when NO cwd file exists).
/// 5. `./config.toml`  (last-resort, current working directory).
///
/// The cwd check stops at the first hit — a present cwd file
/// short-circuits the user-level XDG lookup so the two layers
/// never mix in memory. This is the operator's "use these exact
/// settings for this run" signal.
fn default_config_path() -> Option<PathBuf> {
    tracing::trace!("default_config_path: enter");
    if let Ok(env) = std::env::var("MOAGAN_CONFIG") {
        let path = PathBuf::from(env);
        tracing::trace!(path = %path.display(), "default_config_path: from MOAGAN_CONFIG");
        return Some(path);
    }
    for cwd_candidate in ["moagan.toml", ".moagan.toml"] {
        let p = PathBuf::from(cwd_candidate);
        if p.exists() {
            tracing::trace!(path = %p.display(), "default_config_path: from cwd candidate");
            return Some(p);
        }
    }
    if let Some(proj) = directories::ProjectDirs::from("", "", "moagan") {
        let p = proj.config_dir().join("config.toml");
        tracing::trace!(path = %p.display(), "default_config_path: from ProjectDirs");
        return Some(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home)
            .join(".config")
            .join("moagan")
            .join("config.toml");
        tracing::trace!(path = %p.display(), "default_config_path: from $HOME");
        return Some(p);
    }
    tracing::trace!("default_config_path: falling back to ./config.toml");
    Some(PathBuf::from("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test in this module that mutates process-wide
    /// state (`MOAGAN_CONFIG`, `HOME`, `XDG_CONFIG_HOME`, current
    /// working directory). Acquired at the top of each such test;
    /// released on Drop.
    static TEST_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that snapshots the current working directory on
    /// creation and restores it on Drop. Used by the cwd-precedence
    /// tests so a failed assertion cannot leak the changed cwd into
    /// subsequent tests running on the same thread.
    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().expect("current_dir"),
            }
        }
        fn chdir(&self, p: &std::path::Path) {
            std::env::set_current_dir(p).expect("set_current_dir");
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    /// RAII guard that snapshots selected env vars on creation and
    /// restores them on Drop. Used so a failed assertion cannot leak
    /// mutated env vars into the next test on the same thread.
    /// Includes `XDG_CONFIG_HOME` because the `directories` crate
    /// uses it in preference to `HOME` when computing the XDG config
    /// dir; pinning it makes the XDG-fallback tests deterministic on
    /// CI runners that may have `XDG_CONFIG_HOME` exported.
    struct EnvGuard {
        moagan_config: Option<String>,
        home: Option<std::ffi::OsString>,
        xdg_config_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                moagan_config: std::env::var("MOAGAN_CONFIG").ok(),
                home: std::env::var_os("HOME"),
                xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.moagan_config {
                Some(v) => unsafe {
                    std::env::set_var("MOAGAN_CONFIG", v);
                },
                None => unsafe {
                    std::env::remove_var("MOAGAN_CONFIG");
                },
            }
            match &self.home {
                Some(v) => unsafe {
                    std::env::set_var("HOME", v);
                },
                None => unsafe {
                    std::env::remove_var("HOME");
                },
            }
            match &self.xdg_config_home {
                Some(v) => unsafe {
                    std::env::set_var("XDG_CONFIG_HOME", v);
                },
                None => unsafe {
                    std::env::remove_var("XDG_CONFIG_HOME");
                },
            }
        }
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.max_parallelism, 4);
        assert_eq!(cfg.sketch_timeout_secs, 120);
        assert_eq!(cfg.phase_timeout_secs, 0);
        assert_eq!(cfg.total_timeout_secs, 0);
        assert!(cfg.redact_in_telemetry);
        // v0.10 canonical sections (per-model aliases collapsed into
        // the canonical provider family).
        assert!(cfg.providers.contains_key("minimax"));
        assert!(cfg.providers.contains_key("opencode"));
        assert!(cfg.providers.contains_key("deepseek"));
        assert!(cfg.providers.contains_key("mock"));
    }

    #[test]
    fn server_config_defaults_match_v4_section_8_8() {
        let cfg = Config::default();
        assert_eq!(cfg.server.port, 4096);
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.io_timeout_secs, 30);
        assert!(cfg.server.ensure_home);
    }

    #[test]
    fn retention_config_defaults_match_proposal_section_12() {
        let cfg = Config::default();
        assert_eq!(cfg.retention.keep_runs_days, 30);
        assert_eq!(cfg.retention.keep_runs_count, 100);
        assert_eq!(cfg.retention.max_storage_bytes, 50 * 1024 * 1024 * 1024);
        assert_eq!(cfg.retention.policy, "delete");
    }

    #[test]
    fn toml_round_trip_preserves_server_and_retention() {
        let cfg = Config::default();
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.server.port, cfg.server.port);
        assert_eq!(back.server.host, cfg.server.host);
        assert_eq!(back.retention.keep_runs_days, cfg.retention.keep_runs_days);
        assert_eq!(back.retention.policy, cfg.retention.policy);
    }

    #[test]
    fn provider_lookup() {
        let cfg = Config::default();
        assert!(cfg.provider("minimax").is_ok());
        assert!(cfg.provider("mock").is_ok());
        assert!(cfg.provider("does-not-exist").is_err());
    }

    /// PR-C1 (cluster C, JSON robustness): the helper defaults the
    /// repair path to ON for the `discover` subcommand so the
    /// matrix fan-out (40-500 sketches) amortises the extra LLM
    /// call. Pin the per-mode decision matrix.
    #[test]
    fn json_repair_v2_enabled_for_mode_true_for_discovery() {
        let cfg = Config::default();
        assert!(
            !cfg.llm.json_repair_v2_enabled,
            "fixture: the typed config field starts at false so the helper \
             can override it for discover without a config-file change"
        );
        assert!(cfg.json_repair_v2_enabled_for_mode("discover"));
    }

    /// Other modes honour the typed config flag (default `false`),
    /// so a 4-sketch pipeline never pays the extra LLM-call cost
    /// unless the operator opts in.
    #[test]
    fn json_repair_v2_enabled_for_mode_false_for_other_modes_by_default() {
        let cfg = Config::default();
        for mode in ["fast", "standard", "deep", "explore", "batch"] {
            assert!(
                !cfg.json_repair_v2_enabled_for_mode(mode),
                "mode {mode} should default to off when the typed flag is false"
            );
        }
    }

    /// When the operator explicitly sets
    /// `llm.json_repair_v2_enabled = true`, every non-`discover`
    /// mode picks it up. `discover` is already on regardless, so
    /// the explicit flag is idempotent for it.
    #[test]
    fn json_repair_v2_enabled_for_mode_true_for_other_modes_when_explicitly_set() {
        let mut cfg = Config::default();
        cfg.llm.json_repair_v2_enabled = true;
        for mode in ["fast", "standard", "deep", "explore", "batch"] {
            assert!(
                cfg.json_repair_v2_enabled_for_mode(mode),
                "mode {mode} should follow the explicit typed flag"
            );
        }
        // Discover is already true regardless of the explicit flag.
        assert!(cfg.json_repair_v2_enabled_for_mode("discover"));
    }

    /// The env-var override path (`MOAGAN_JSON_REPAIR_V2_ENABLED`)
    /// populates the same typed field, so the helper sees it via
    /// `apply_env_overrides` and propagates to non-`discover`
    /// modes. Pin the wiring end-to-end: set the env var, run
    /// `apply_env_overrides`, and assert the helper agrees.
    #[test]
    fn json_repair_v2_enabled_for_mode_env_var_override() {
        unsafe {
            std::env::set_var("MOAGAN_JSON_REPAIR_V2_ENABLED", "true");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert!(
            cfg.llm.json_repair_v2_enabled,
            "env var should have flipped the typed field via apply_env_overrides"
        );
        // Non-`discover` modes pick up the flag via the helper.
        assert!(cfg.json_repair_v2_enabled_for_mode("standard"));
        // `discover` is on regardless — the env var is a no-op for it.
        assert!(cfg.json_repair_v2_enabled_for_mode("discover"));
        unsafe {
            std::env::remove_var("MOAGAN_JSON_REPAIR_V2_ENABLED");
        }
    }

    /// Unknown mode strings (corrupted manifest, future mode
    /// without a helper update) fall through to the typed flag so
    /// the default behaviour is preserved.
    #[test]
    fn json_repair_v2_enabled_for_mode_unknown_mode_falls_back_to_typed_flag() {
        let cfg = Config::default();
        assert!(!cfg.json_repair_v2_enabled_for_mode("unknown"));
        let mut cfg = Config::default();
        cfg.llm.json_repair_v2_enabled = true;
        assert!(cfg.json_repair_v2_enabled_for_mode("unknown"));
    }

    #[test]
    fn tomllib_round_trip() {
        let cfg = Config::default();
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.max_parallelism, cfg.max_parallelism);
        assert_eq!(back.default_provider, cfg.default_provider);
        assert_eq!(back.providers.len(), cfg.providers.len());
    }

    #[test]
    fn env_overrides_parallelism() {
        unsafe {
            std::env::set_var("MOAGAN_MAX_PARALLELISM", "12");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.max_parallelism, 12);
        unsafe {
            std::env::remove_var("MOAGAN_MAX_PARALLELISM");
        }
    }

    #[test]
    fn env_overrides_minimax_endpoint() {
        // Serialised against every other test in this module that
        // mutates the `MOAGAN_MINIMAX_*` env vars (and against any
        // out-of-module test that touches them). Without the lock,
        // a parallel test flipping the var between `set_var` and
        // `apply_env_overrides` would race the override.
        let _lock = crate::TEST_MINIMAX_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Default config has the hardcoded production endpoint.
        let mut cfg = Config::default();
        let baseline = cfg
            .providers
            .get("minimax")
            .unwrap()
            .endpoint
            .clone()
            .expect("default minimax section must carry endpoint");
        assert_eq!(baseline, "https://api.minimax.io/anthropic/v1/messages");

        // With the env var set, apply_env_overrides rewrites the
        // minimax section (the only one whose endpoint matches the
        // MiniMax /v1/messages URL pattern) but leaves other
        // providers (e.g. "mock", "opencode") alone.
        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_ENDPOINT", "http://localhost:8086/x");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MINIMAX_ENDPOINT");
        }
        assert_eq!(
            cfg.providers.get("minimax").unwrap().endpoint,
            Some("http://localhost:8086/x".to_owned())
        );
        assert_eq!(
            cfg.providers.get("mock").unwrap().endpoint,
            Some("mock://local".to_owned()),
            "non-minimax providers must not be touched by MOAGAN_MINIMAX_ENDPOINT"
        );
    }

    /// `MOAGAN_MINIMAX_MODEL` mirrors the existing `MOAGAN_MINIMAX_ENDPOINT`
    /// override: applied to every provider whose kind is `minimax`, leaves
    /// non-minimax providers alone. Operators use it to retarget the
    /// default model without registering a new provider entry in
    /// `config.toml` (parity with Q5: 4 canonical models are already
    /// registered, but env-driven override lets tests + CI pin a
    /// different default).
    #[test]
    fn env_overrides_minimax_model() {
        // Serialised against `env_overrides_minimax_model_ignores_blank`,
        // `env_overrides_minimax_endpoint`, and any out-of-module test
        // that touches the same env vars. Without the lock, parallel
        // test threads race on the shared `MOAGAN_MINIMAX_MODEL` value.
        let _lock = crate::TEST_MINIMAX_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let mut cfg = Config::default();
        // Baseline: the canonical `minimax` section carries the
        // first model id on `models[0].id` (the v0.10 canonical
        // field).
        assert_eq!(
            cfg.providers.get("minimax").unwrap().models[0].id,
            "MiniMax-M3"
        );

        // Set, exercise, restore — every step inside the lock so
        // a parallel test on another thread cannot see a half-applied
        // state. Drop order guarantees the env var is restored even
        // when an assertion panics.
        let prev = std::env::var("MOAGAN_MINIMAX_MODEL").ok();
        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_MODEL", "MiniMax-M2.5");
        }
        cfg.apply_env_overrides();
        match prev {
            Some(v) => unsafe {
                std::env::set_var("MOAGAN_MINIMAX_MODEL", v);
            },
            None => unsafe {
                std::env::remove_var("MOAGAN_MINIMAX_MODEL");
            },
        }

        // Only the canonical `minimax` section reflects the env
        // value. The opencode-routed `minimax-m3/m2.7/m2.5` are
        // models on the `opencode` section now and must NOT be
        // touched by the MOAGAN_MINIMAX_MODEL env override.
        assert_eq!(
            cfg.providers.get("minimax").unwrap().models[0].id,
            "MiniMax-M2.5",
            "minimax section should pick up MOAGAN_MINIMAX_MODEL"
        );
        let oc_spec = cfg.providers.get("opencode").unwrap();
        for model_id in ["minimax-m3", "minimax-m2.7", "minimax-m2.5"] {
            let entry = oc_spec
                .models
                .iter()
                .find(|m| m.id == model_id)
                .unwrap_or_else(|| panic!("opencode section must contain {model_id}"));
            assert_eq!(
                entry.id, model_id,
                "opencode model {model_id} must NOT pick up MOAGAN_MINIMAX_MODEL"
            );
        }
        // The mock provider must not be touched. v0.10 ships
        // `[providers.mock] models = [{ id = "mock-model" }]` from
        // `default_providers()` so the dispatcher accepts
        // `--provider mock:mock-model` without a per-test
        // `MOAGAN_CONFIG` workaround.
        assert_eq!(
            cfg.providers.get("mock").unwrap().models,
            vec![ModelConfig {
                id: "mock-model".to_owned(),
                endpoint: None,
                max_tokens: None,
            }],
            "mock provider must not be touched by MOAGAN_MINIMAX_MODEL"
        );
    }

    /// Empty / whitespace env values are ignored, so a stale export in
    /// the shell does not blank the configured model. Mirrors the
    /// `MOAGAN_MINIMAX_ENDPOINT` handling.
    #[test]
    fn env_overrides_minimax_model_ignores_blank() {
        // Serialised against `env_overrides_minimax_model` and every
        // other test that touches the shared `MOAGAN_MINIMAX_*` env
        // vars.
        let _lock = crate::TEST_MINIMAX_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let mut cfg = Config::default();
        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_MODEL", "   ");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MINIMAX_MODEL");
        }
        // Blank value: the first model id stays at the canonical
        // v0.10 default.
        assert_eq!(
            cfg.providers.get("minimax").unwrap().models[0].id,
            "MiniMax-M3"
        );
    }

    /// Q6 pin: DeepSeek is exposed with its canonical default model
    /// on the v0.10 schema (`models[0].id`).
    #[test]
    fn default_providers_lists_deepseek() {
        let cfg = Config::default();
        let spec = cfg
            .providers
            .get("deepseek")
            .expect("deepseek missing from default providers");
        assert_eq!(
            spec.endpoint.as_deref(),
            Some("https://api.deepseek.com/v1/chat/completions")
        );
        assert!(
            spec.models.iter().any(|m| m.id == "deepseek-chat"),
            "deepseek section must list `deepseek-chat` in models[]"
        );
    }

    /// Q7 pin: the canonical `opencode` section groups every
    /// OpenCode model under one roof. Each model carries its own
    /// fully-qualified URL so the dispatcher can pick the wire
    /// format from the path. The v0.9 per-model alias sections
    /// (`kimi-k3`, `minimax-m3`, `gpt-5.6-luna`, …) are gone —
    /// callers reach them via `--provider opencode:MODEL`.
    #[test]
    fn default_providers_lists_opencode_aliases() {
        let cfg = Config::default();
        let spec = cfg
            .providers
            .get("opencode")
            .expect("opencode section missing from default providers");
        // One representative model per wire-format group; the URL
        // lives on the per-model `endpoint` field, NOT on the
        // section (every model on this section has its own URL).
        let representatives = [
            ("kimi-k3", "https://opencode.ai/zen/go/v1/chat/completions"),
            ("minimax-m3", "https://opencode.ai/zen/go/v1/messages"),
            ("gpt-5.6-luna", "https://opencode.ai/zen/go/v1/responses"),
        ];
        for (model_id, expected_endpoint) in representatives {
            let model = spec
                .models
                .iter()
                .find(|m| m.id == model_id)
                .unwrap_or_else(|| panic!("opencode section must contain model {model_id}"));
            assert_eq!(
                model.endpoint.as_deref(),
                Some(expected_endpoint),
                "opencode model {model_id} must carry the full wire-format URL"
            );
        }
    }

    /// v0.10 schema pin: every default provider exposes its model
    /// list as `models[].id`. The dispatcher iterates this list to
    /// build one `Provider` per `(section, model)` pair.
    #[test]
    fn default_providers_models_field_populated() {
        let cfg = Config::default();
        // Sample representatives from each canonical section.
        let samples = [
            ("minimax", "MiniMax-M3"),
            ("deepseek", "deepseek-chat"),
            ("opencode", "kimi-k3"),
        ];
        for (section, expected_id) in samples {
            let spec = cfg
                .providers
                .get(section)
                .unwrap_or_else(|| panic!("section {section} missing from default providers"));
            assert!(
                !spec.models.is_empty(),
                "section {section} must carry at least one ModelConfig entry"
            );
            assert!(
                spec.models.iter().any(|m| m.id == expected_id),
                "section {section} must carry {expected_id} in models[]"
            );
        }
    }

    /// Q5 + 2026-08-04 pin: the canonical MiniMax models are
    /// listed on the canonical `minimax` section (direct MiniMax)
    /// AND on the `opencode` section (subscription, on the
    /// `/v1/messages` endpoint, mirroring the v0.9 `minimax-m3`
    /// alias). The split matches the operator's 2026-08-04 model
    /// roster: `minimax-m3/m2.7/m2.5` are routed through
    /// OpenCode.
    #[test]
    fn default_providers_lists_four_canonical_minimax_models() {
        let cfg = Config::default();
        let canonical_direct = [
            "MiniMax-M3",
            "MiniMax-M2.7",
            "MiniMax-M2.7-highspeed",
            "MiniMax-M2.5",
        ];
        let minimax_spec = cfg
            .providers
            .get("minimax")
            .expect("minimax section missing from default providers");
        for model_id in canonical_direct {
            assert!(
                minimax_spec.models.iter().any(|m| m.id == model_id),
                "minimax section must carry {model_id} on models[]"
            );
        }
        let canonical_opencode = ["minimax-m3", "minimax-m2.7", "minimax-m2.5"];
        let opencode_spec = cfg
            .providers
            .get("opencode")
            .expect("opencode section missing from default providers");
        for model_id in canonical_opencode {
            assert!(
                opencode_spec.models.iter().any(|m| m.id == model_id),
                "opencode section must carry {model_id} on models[]"
            );
        }
    }

    /// Pin: every default `ProviderConfig` for an OpenCode model
    /// registered in `default_providers()` enables the auto-probe.
    /// The per-model `max_tokens` ceiling lives on
    /// `models[].max_tokens` (v0.10 canonical); the probe narrows
    /// it to the real upstream boundary.
    #[test]
    fn default_opencode_providers_enable_max_tokens_auto_probe() {
        let cfg = Config::default();
        let oc_spec = cfg
            .providers
            .get("opencode")
            .expect("opencode section missing from default providers");
        // Section-level probe floor must be > 0 so the
        // startup auto-probe fires.
        let floor = oc_spec.max_token_auto.unwrap_or(0);
        assert!(
            floor > 0,
            "opencode section must enable the max_tokens auto-probe \
             (max_token_auto = Some(n), n > 0); with the probe off and \
             max_tokens = 1M the upstream rejects with HTTP 400"
        );
        // Every opencode model starts at DEFAULT_MAX_TOKENS so
        // the probe has a generous floor to work from.
        for model in &oc_spec.models {
            assert_eq!(
                model.max_tokens,
                Some(DEFAULT_MAX_TOKENS),
                "opencode model {} must start at DEFAULT_MAX_TOKENS; \
                 the probe narrows it to the real ceiling",
                model.id
            );
        }
    }

    /// Pin: the canonical `minimax` section starts at
    /// `DEFAULT_MAX_TOKENS` and enables the auto-probe.
    ///
    /// The MiniMax Anthropic-compatible upstream answers HTTP 400
    /// ("model[MiniMax-M3] does not support max tokens > 524288"), so
    /// the wire layer (`MinimaxProvider::send`) clamps
    /// unconditionally to `MINIMAX_MAX_TOKENS_CAP`; the config
    /// default starts high and the probe discovers the real ceiling.
    #[test]
    fn default_minimax_provider_enables_max_tokens_auto_probe() {
        let cfg = Config::default();
        let spec = cfg
            .providers
            .get("minimax")
            .expect("minimax section missing from default providers");
        assert_eq!(
            spec.models[0].max_tokens,
            Some(DEFAULT_MAX_TOKENS),
            "minimax section first model must start at DEFAULT_MAX_TOKENS; \
             the probe narrows it to the real ceiling"
        );
        let floor = spec.max_token_auto.unwrap_or(0);
        assert!(
            floor > 0,
            "minimax section must enable the max_tokens auto-probe \
             (max_token_auto = Some(n), n > 0)"
        );
    }

    /// `MOAGAN_MAX_TOKEN_AUTO=0` is the operator kill-switch: it maps
    /// to the `Some(0)` sentinel on *every* provider, which the
    /// registry treats exactly like `None` when deciding whether to
    /// attach a `MaxTokensTable`. Pin the config half here; the
    /// registry half is pinned in
    /// `llm::provider::tests::env_max_token_auto_zero_disables_probe`.
    #[test]
    fn env_max_token_auto_zero_disables_probe() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_MAX_TOKEN_AUTO", "0");
        }
        let mut cfg = Config::default();
        // Pre-condition: the real providers ship with the probe on,
        // so the assertion below cannot pass vacuously.
        assert!(
            cfg.providers
                .values()
                .any(|s| s.max_token_auto.is_some_and(|n| n > 0)),
            "default config must enable the probe somewhere"
        );
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MAX_TOKEN_AUTO");
        }
        for (name, spec) in &cfg.providers {
            assert_eq!(
                spec.max_token_auto,
                Some(0),
                "provider {name} should carry the Some(0) off-sentinel"
            );
        }
    }

    /// The other spellings of "off" collapse to the same sentinel,
    /// and a numeric value is taken verbatim as the floor.
    #[test]
    fn env_max_token_auto_parses_off_aliases_and_numbers() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for off in ["", "off", "false", "0"] {
            unsafe {
                std::env::set_var("MOAGAN_MAX_TOKEN_AUTO", off);
            }
            let mut cfg = Config::default();
            cfg.apply_env_overrides();
            assert_eq!(
                cfg.providers["minimax"].max_token_auto,
                Some(0),
                "{off:?} should map to the off sentinel"
            );
        }
        unsafe {
            std::env::set_var("MOAGAN_MAX_TOKEN_AUTO", "8192");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.providers["minimax"].max_token_auto, Some(8192));
        // Garbage parses to `None`, which is also "off".
        unsafe {
            std::env::set_var("MOAGAN_MAX_TOKEN_AUTO", "not-a-number");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.providers["minimax"].max_token_auto, None);
        unsafe {
            std::env::remove_var("MOAGAN_MAX_TOKEN_AUTO");
        }
    }

    /// `MOAGAN_MAX_TOKEN_AUTO_SAVE` flips persistence for every
    /// provider; anything outside the truthy set turns it off.
    #[test]
    fn env_max_token_auto_save_override() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_MAX_TOKEN_AUTO_SAVE", "false");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert!(
            cfg.providers.values().all(|s| !s.max_token_auto_save),
            "`false` must disable persistence everywhere"
        );
        unsafe {
            std::env::set_var("MOAGAN_MAX_TOKEN_AUTO_SAVE", "yes");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert!(
            cfg.providers.values().all(|s| s.max_token_auto_save),
            "`yes` must enable persistence everywhere"
        );
        unsafe {
            std::env::remove_var("MOAGAN_MAX_TOKEN_AUTO_SAVE");
        }
    }

    /// The default for `max_token_auto_save` is `true`, and a TOML
    /// provider table that omits both new keys still deserialises
    /// (the fields are additive).
    #[test]
    fn provider_config_max_token_auto_defaults_are_additive() {
        assert!(default_max_token_auto_save());
        let spec: ProviderConfig = toml::from_str(
            r#"
            kind = "minimax"
            endpoint = "https://example.invalid/v1/messages"
            model = "MiniMax-M3"
            "#,
        )
        .expect("a provider table without the new keys must still parse");
        assert_eq!(spec.max_token_auto, None);
        assert!(spec.max_token_auto_save);
    }

    // -----------------------------------------------------------------------
    // Issue #657 fix #3 — `MOAGAN_TEMPERATURE_AUTO` + `temperature_auto_enabled`
    // -----------------------------------------------------------------------

    /// Issue #657 fix #3: `MOAGAN_TEMPERATURE_AUTO=false` flips
    /// `temperature_auto_enabled` to `Some(false)` on every
    /// provider (mirror of `MOAGAN_MAX_TOKEN_AUTO`). The runtime
    /// uses this signal to skip the 21-request background
    /// temperature probe fan-out (`src/llm/provider.rs`).
    #[test]
    fn env_temperature_auto_false_disables_probe() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_TEMPERATURE_AUTO", "false");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_TEMPERATURE_AUTO");
        }
        for (name, spec) in &cfg.providers {
            assert_eq!(
                spec.temperature_auto_enabled,
                Some(false),
                "provider {name} must carry temperature_auto_enabled = Some(false) under MOAGAN_TEMPERATURE_AUTO=false"
            );
        }
    }

    /// The other "off" spellings collapse to the same off-sentinel
    /// (`Some(false)`), mirroring the `MOAGAN_<name>_OMIT_MAX_TOKENS`
    /// convention.
    #[test]
    fn env_temperature_auto_parses_off_aliases() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for off in ["false", "0", "no", "off"] {
            unsafe {
                std::env::set_var("MOAGAN_TEMPERATURE_AUTO", off);
            }
            let mut cfg = Config::default();
            cfg.apply_env_overrides();
            assert_eq!(
                cfg.providers["minimax"].temperature_auto_enabled,
                Some(false),
                "{off:?} should map to temperature_auto_enabled = Some(false)"
            );
        }
        unsafe {
            std::env::remove_var("MOAGAN_TEMPERATURE_AUTO");
        }
    }

    /// Truthy spellings set `Some(true)`; the runtime currently
    /// treats `Some(true)` and `None` identically (probe on), so
    /// this is mostly a TOML-roundtrip guard, not a behaviour
    /// switch. Pin the shape so a future refactor cannot silently
    /// conflate the two.
    #[test]
    fn env_temperature_auto_parses_truthy_aliases() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for yes in ["true", "1", "yes", "on"] {
            unsafe {
                std::env::set_var("MOAGAN_TEMPERATURE_AUTO", yes);
            }
            let mut cfg = Config::default();
            cfg.apply_env_overrides();
            assert_eq!(
                cfg.providers["minimax"].temperature_auto_enabled,
                Some(true),
                "{yes:?} should map to temperature_auto_enabled = Some(true)"
            );
        }
        unsafe {
            std::env::remove_var("MOAGAN_TEMPERATURE_AUTO");
        }
    }

    /// Unset env: the field stays `None` (the registry default —
    /// probe on). The pre-fix behaviour had no env var at all, so
    /// this is the regression guard.
    #[test]
    fn env_temperature_auto_unset_leaves_field_none() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("MOAGAN_TEMPERATURE_AUTO");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        for (name, spec) in &cfg.providers {
            assert_eq!(
                spec.temperature_auto_enabled, None,
                "provider {name} must keep temperature_auto_enabled = None when env is unset"
            );
        }
    }

    /// Unrecognised values leave the field alone so a typo does
    /// not silently flip the probe off (same convention as
    /// `MOAGAN_<name>_OMIT_MAX_TOKENS`).
    #[test]
    fn env_temperature_auto_unrecognised_is_ignored() {
        let _guard = TEST_CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_TEMPERATURE_AUTO", "not-a-bool");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_TEMPERATURE_AUTO");
        }
        for (name, spec) in &cfg.providers {
            assert_eq!(
                spec.temperature_auto_enabled, None,
                "unrecognised value must not flip the field; provider {name}"
            );
        }
    }

    /// TOML round-trip: an operator who writes
    /// `temperature_auto_enabled = false` in `[providers.<name>]`
    /// deserialises to `Some(false)` so the runtime sees the
    /// opt-out signal directly (no env var needed).
    #[test]
    fn provider_config_temperature_auto_enabled_parses() {
        let spec: ProviderConfig = toml::from_str(
            r#"
            endpoint = "https://example.invalid/v1/messages"
            temperature_auto_enabled = false
            "#,
        )
        .expect("a provider table with temperature_auto_enabled=false must parse");
        assert_eq!(spec.temperature_auto_enabled, Some(false));
    }

    /// Default `None`: a TOML provider table that omits
    /// `temperature_auto_enabled` deserialises with the field at
    /// `None`, matching the v0.12.x registry default (probe on).
    #[test]
    fn provider_config_temperature_auto_enabled_defaults_to_none() {
        let spec: ProviderConfig = toml::from_str(
            r#"
            endpoint = "https://example.invalid/v1/messages"
            "#,
        )
        .expect("a provider table without temperature_auto_enabled must still parse");
        assert_eq!(spec.temperature_auto_enabled, None);
    }

    /// v0.10 schema pin: an operator who writes `models = [{ id = ... }]`
    /// in `config.toml` gets the new structure parsed verbatim. The
    /// deprecated `model` / `kind` / `hard_incompatibilities` fields
    /// remain optional so legacy TOML files keep loading.
    #[test]
    fn provider_config_models_field_parses() {
        let spec: ProviderConfig = toml::from_str(
            r#"
            endpoint = "https://opencode.ai/zen/go/v1"

            [[models]]
            id = "kimi-k3"
            max_tokens = 1000000

            [[models]]
            id = "minimax-m3"
            max_tokens = 1000000
            "#,
        )
        .expect("a provider table with models[] must parse");
        assert_eq!(spec.models.len(), 2);
        assert_eq!(spec.models[0].id, "kimi-k3");
        assert_eq!(spec.models[0].max_tokens, Some(1_000_000));
        assert_eq!(spec.models[1].id, "minimax-m3");
        assert_eq!(spec.models[1].max_tokens, Some(1_000_000));
    }

    /// `ModelConfig::default()` round-trip: serde-defaults match the
    /// struct's `Default` impl so an empty `[[models]]` entry still
    /// produces a usable `ModelConfig` (id is empty — callers must
    /// populate it explicitly).
    #[test]
    fn model_config_default_round_trip() {
        let spec: ModelConfig = toml::from_str("").expect("empty ModelConfig must parse");
        assert_eq!(spec.id, "");
        assert_eq!(spec.endpoint, None);
        assert_eq!(spec.max_tokens, None);
        assert_eq!(spec, ModelConfig::default());
    }

    /// Catalog §D.19.5 default knobs: 5 errors in 60 s -> open for
    /// 30 s. Pin the defaults so a refactor that drops the catalog
    /// alignment trips the test before it lands in production.
    #[test]
    fn circuit_breaker_defaults_match_catalog_d_19_5() {
        let cfg = CircuitBreakerConfig::default();
        assert_eq!(cfg.threshold, 5);
        assert_eq!(cfg.window_secs, 60);
        assert_eq!(cfg.cooldown_secs, 30);
    }

    /// The breaker knobs must survive a TOML round-trip so
    /// operators can pin their values in `~/.config/moagan/config.toml`.
    #[test]
    fn circuit_breaker_toml_round_trip() {
        let cfg = Config {
            circuit_breaker: CircuitBreakerConfig {
                threshold: 3,
                window_secs: 30,
                cooldown_secs: 120,
            },
            ..Config::default()
        };
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.circuit_breaker.threshold, 3);
        assert_eq!(back.circuit_breaker.window_secs, 30);
        assert_eq!(back.circuit_breaker.cooldown_secs, 120);
    }

    /// `Config::startup_reconcile` defaults to `true`. Track F
    /// (D.28.3 + D.28.4) auto-runs the reconcile pass at the top
    /// of every dispatcher entry unless the operator opts out.
    #[test]
    fn startup_reconcile_default_is_true() {
        let cfg = Config::default();
        assert!(cfg.startup_reconcile);
    }

    /// Per-test mutex shared by the env-override tests so they
    /// cannot race each other on the same `MOAGAN_*` variable.
    /// Lives next to the tests for documentation visibility.
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `MOAGAN_STARTUP_RECONCILE=false` flips the flag via the
    /// env-override path so a CI shell can opt out without
    /// editing `config.toml`. The mutex lock avoids a parallel
    /// race with `env_var_startup_reconcile_true_is_noop` (both
    /// mutate the same env var).
    #[test]
    fn env_var_startup_reconcile_false_disables() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_STARTUP_RECONCILE", "false");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_STARTUP_RECONCILE");
        }
        assert!(!cfg.startup_reconcile);
    }

    /// `MOAGAN_STARTUP_RECONCILE=true` is the no-op path; a
    /// TOML-default `true` survives the env round-trip. Locked
    /// against the `false` test above (parallel runs otherwise
    /// race the env var).
    #[test]
    fn env_var_startup_reconcile_true_is_noop() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_STARTUP_RECONCILE", "true");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_STARTUP_RECONCILE");
        }
        assert!(cfg.startup_reconcile);
    }

    /// Garbage / whitespace env values are ignored so a stale
    /// export does not silently flip the flag.
    #[test]
    fn env_var_startup_reconcile_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_STARTUP_RECONCILE", "   ");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_STARTUP_RECONCILE");
        }
        assert!(
            cfg.startup_reconcile,
            "garbage env must not flip the default"
        );
    }

    /// Catalog §D.11.9: the default value of `sandbox_allow_network`
    /// is `false` (off-by-default). This is the privacy / hermetic-
    /// sandbox contract — every operator install should run without
    /// the sandbox reaching the network.
    #[test]
    fn config_sandbox_allow_network_default_is_false() {
        let cfg = Config::default();
        assert!(
            !cfg.sandbox_allow_network,
            "sandbox_allow_network must default to false (D.11.9 off-by-default)"
        );
    }

    /// Catalog §D.11.9: `MOAGAN_SANDBOX_ALLOW_NETWORK=true` flips
    /// the flag. Locked against the `_false` and `_garbage` tests
    /// below because they all mutate the same env var.
    #[test]
    fn env_var_sandbox_allow_network_true_overrides_default() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_ALLOW_NETWORK", "true");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_ALLOW_NETWORK");
        }
        assert!(
            cfg.sandbox_allow_network,
            "MOAGAN_SANDBOX_ALLOW_NETWORK=true must opt in"
        );
    }

    /// Catalog §D.11.9: `MOAGAN_SANDBOX_ALLOW_NETWORK=false` on a
    /// custom-true config resets the flag (the env override is the
    /// canonical mechanism to flip the default in either direction).
    #[test]
    fn env_var_sandbox_allow_network_false_overrides_true() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut cfg = Config {
            sandbox_allow_network: true,
            ..Config::default()
        };
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_ALLOW_NETWORK", "false");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_ALLOW_NETWORK");
        }
        assert!(
            !cfg.sandbox_allow_network,
            "MOAGAN_SANDBOX_ALLOW_NETWORK=false must opt out"
        );
    }

    /// Catalog §D.11.9: garbage / whitespace env values are ignored
    /// so a stray export does not silently flip the default.
    #[test]
    fn env_var_sandbox_allow_network_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_ALLOW_NETWORK", "   ");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_ALLOW_NETWORK");
        }
        assert!(
            !cfg.sandbox_allow_network,
            "garbage env must not flip the default"
        );
    }

    /// Catalog §D.11.10: the default value of `sandbox_allow_injection`
    /// is `false` (the secret-stripping pass always runs). Operators
    /// opt in via `MOAGAN_SANDBOX_ALLOW_INJECTION=true` or
    /// `moagan run --allow-injection`.
    #[test]
    fn config_sandbox_allow_injection_default_is_false() {
        let cfg = Config::default();
        assert!(
            !cfg.sandbox_allow_injection,
            "sandbox_allow_injection must default to false (D.11.10 strip-by-default)"
        );
    }

    /// Catalog §D.11.10: `MOAGAN_SANDBOX_ALLOW_INJECTION=true` flips
    /// the flag so the sandbox skips the argv-side secret-stripping
    /// pass. Useful for debugging / repro cases.
    #[test]
    fn env_var_sandbox_allow_injection_true_overrides_default() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_ALLOW_INJECTION", "true");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_ALLOW_INJECTION");
        }
        assert!(
            cfg.sandbox_allow_injection,
            "MOAGAN_SANDBOX_ALLOW_INJECTION=true must opt in"
        );
    }

    /// Catalog §D.11.13: `MOAGAN_SANDBOX_NETWORK_POLICY=off` flips
    /// the policy to [`NetworkPolicy::Off`]. Locked against the
    /// `_open` / `_allow_list` / `_garbage` tests below because
    /// they all mutate the same env var.
    #[test]
    fn config_env_var_network_policy_off() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_NETWORK_POLICY", "off");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_NETWORK_POLICY");
        }
        assert!(
            matches!(cfg.sandbox_network_policy, NetworkPolicy::Off),
            "MOAGAN_SANDBOX_NETWORK_POLICY=off must parse to NetworkPolicy::Off, got {:?}",
            cfg.sandbox_network_policy
        );
    }

    /// Catalog §D.11.13: `MOAGAN_SANDBOX_NETWORK_POLICY=open` flips
    /// the policy to [`NetworkPolicy::Open`] so cargo can fetch
    /// crates from the registry. Case-insensitive (`OPEN` is fine).
    #[test]
    fn config_env_var_network_policy_open() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_NETWORK_POLICY", "OPEN");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_NETWORK_POLICY");
        }
        assert!(
            matches!(cfg.sandbox_network_policy, NetworkPolicy::Open),
            "MOAGAN_SANDBOX_NETWORK_POLICY=OPEN must parse to NetworkPolicy::Open, got {:?}",
            cfg.sandbox_network_policy
        );
    }

    /// Catalog §D.11.13: `MOAGAN_SANDBOX_NETWORK_POLICY=["a","b"]`
    /// (JSON array form) parses to
    /// [`NetworkPolicy::AllowList`] with the listed hosts verbatim.
    /// This is the only way to express a partial opt-in from the
    /// shell because the legacy `sandbox_allow_network` boolean
    /// cannot represent an allowlist.
    #[test]
    fn config_env_var_network_policy_allow_list() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var(
                "MOAGAN_SANDBOX_NETWORK_POLICY",
                r#"["crates.io","github.com"]"#,
            );
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_NETWORK_POLICY");
        }
        match cfg.sandbox_network_policy {
            NetworkPolicy::AllowList { ref hosts } => assert_eq!(
                hosts,
                &vec!["crates.io".to_owned(), "github.com".to_owned()],
                "JSON array must parse to AllowList with the listed hosts"
            ),
            other => panic!("expected AllowList, got {other:?}"),
        }
    }

    /// Catalog §D.11.13: garbage / whitespace env values are ignored
    /// so a stale / malformed export does not silently flip the
    /// default. This mirrors the handling of
    /// `MOAGAN_SANDBOX_ALLOW_NETWORK`.
    #[test]
    fn config_env_var_network_policy_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_NETWORK_POLICY", "   ");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_NETWORK_POLICY");
        }
        assert!(
            matches!(cfg.sandbox_network_policy, NetworkPolicy::Off),
            "garbage env must not flip the default Off policy, got {:?}",
            cfg.sandbox_network_policy
        );
    }

    /// Catalog §D.11.13: the default value of `sandbox_network_policy`
    /// is [`NetworkPolicy::Off`] (off-by-default). Pin the default so
    /// a refactor that drops the catalog alignment trips the test
    /// before it lands in production.
    #[test]
    fn config_sandbox_network_policy_default_is_off() {
        let cfg = Config::default();
        assert!(
            matches!(cfg.sandbox_network_policy, NetworkPolicy::Off),
            "sandbox_network_policy must default to Off (D.11.13 off-by-default), got {:?}",
            cfg.sandbox_network_policy
        );
    }

    /// Catalog §D.11.13: the typed `sandbox_network_policy` survives
    /// a TOML round-trip so operators can pin their value in
    /// `~/.config/moagan/config.toml`. The TOML form for an
    /// AllowList follows the internally-tagged enum shape:
    /// `sandbox_network_policy = { kind = "allow_list", hosts = ["a", "b"] }`.
    #[test]
    fn config_sandbox_network_policy_toml_round_trip() {
        let cfg = Config {
            sandbox_network_policy: NetworkPolicy::AllowList {
                hosts: vec!["crates.io".to_owned(), "github.com".to_owned()],
            },
            ..Config::default()
        };
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        match back.sandbox_network_policy {
            NetworkPolicy::AllowList { hosts } => assert_eq!(
                hosts,
                vec!["crates.io".to_owned(), "github.com".to_owned()],
                "TOML round-trip must preserve the AllowList"
            ),
            other => panic!("expected AllowList after round-trip, got {other:?}"),
        }
    }

    #[test]
    fn config_sandbox_namespaces_default_is_empty() {
        assert!(Config::default().sandbox_namespaces.is_empty());
    }

    #[test]
    fn config_env_var_sandbox_namespaces_parses_csv() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_NAMESPACES", "mount,pid,net");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_NAMESPACES");
        }
        assert_eq!(
            cfg.sandbox_namespaces,
            NamespaceFlags::MOUNT | NamespaceFlags::PID | NamespaceFlags::NET
        );
    }

    /// Catalog §D.11.7: the default value of `sandbox_seccomp` is
    /// [`SeccompPolicyKind::Permissive`] (no-op). Pin the default so
    /// a refactor that flips it trips the test before it lands in
    /// production.
    #[test]
    fn config_sandbox_seccomp_default_is_permissive() {
        let cfg = Config::default();
        assert!(
            matches!(cfg.sandbox_seccomp, SeccompPolicyKind::Permissive),
            "sandbox_seccomp must default to Permissive (D.11.7 off-by-default), got {:?}",
            cfg.sandbox_seccomp
        );
    }

    /// Catalog §D.11.7: `MOAGAN_SANDBOX_SECCOMP=strict_rust_build`
    /// flips the knob to [`SeccompPolicyKind::StrictRustBuild`].
    /// Locked against the `_permissive` / `_garbage` tests below
    /// because they all mutate the same env var.
    #[test]
    fn config_env_var_seccomp_strict_rust_build_overrides_default() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_SECCOMP", "strict_rust_build");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_SECCOMP");
        }
        assert!(
            matches!(cfg.sandbox_seccomp, SeccompPolicyKind::StrictRustBuild),
            "MOAGAN_SANDBOX_SECCOMP=strict_rust_build must opt in, got {:?}",
            cfg.sandbox_seccomp
        );
    }

    /// Catalog §D.11.7: case-insensitive parsing — the env var
    /// accepts `STRICT_RUST_BUILD`, `Permissive`, etc. Pin the
    /// `PERMISSIVE` form on top of the default so a refactor that
    /// tightens the parser surfaces as a test failure.
    #[test]
    fn config_env_var_seccomp_permissive_overrides_default() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut cfg = Config {
            sandbox_seccomp: SeccompPolicyKind::StrictRustBuild,
            ..Config::default()
        };
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_SECCOMP", "PERMISSIVE");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_SECCOMP");
        }
        assert!(
            matches!(cfg.sandbox_seccomp, SeccompPolicyKind::Permissive),
            "MOAGAN_SANDBOX_SECCOMP=PERMISSIVE must reset to Permissive, got {:?}",
            cfg.sandbox_seccomp
        );
    }

    /// Catalog §D.11.7: garbage / whitespace env values are ignored
    /// so a stale / malformed export does not silently flip the
    /// default. Mirrors the handling of
    /// `MOAGAN_SANDBOX_NETWORK_POLICY`.
    #[test]
    fn config_env_var_seccomp_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_SECCOMP", "   ");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_SECCOMP");
        }
        assert!(
            matches!(cfg.sandbox_seccomp, SeccompPolicyKind::Permissive),
            "garbage env must not flip the default Permissive, got {:?}",
            cfg.sandbox_seccomp
        );
    }

    /// Catalog §D.11.7: the typed `sandbox_seccomp` survives a TOML
    /// round-trip so operators can pin their value in
    /// `~/.config/moagan/config.toml` with
    /// `sandbox_seccomp = "strict_rust_build"`.
    #[test]
    fn config_sandbox_seccomp_toml_round_trip() {
        let cfg = Config {
            sandbox_seccomp: SeccompPolicyKind::StrictRustBuild,
            ..Config::default()
        };
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert!(
            matches!(back.sandbox_seccomp, SeccompPolicyKind::StrictRustBuild),
            "TOML round-trip must preserve StrictRustBuild, got {:?}",
            back.sandbox_seccomp
        );
    }

    // ----------------------------------------------------------------
    // D.11.1 — cgroup v2 + prlimit fallback tests.
    //
    // Each test below covers one of the env-override / TOML wire
    // contracts spelled out in the PR-B.1 spec. The default value
    // test is synchronous so it runs even when the tokio runtime is
    // unavailable; the env-var / TOML round-trip tests acquire
    // `TEST_ENV_LOCK` so they cannot race each other on the same
    // `MOAGAN_SANDBOX_CGROUP` variable.
    // ----------------------------------------------------------------

    /// Catalog §D.11.1: the default value of `sandbox_cgroup` is
    /// `None` (no kernel-level resource cap) so the default install
    /// is unaffected by this PR. Pin the default so a refactor that
    /// flips it trips the test before it lands in production.
    #[test]
    fn config_sandbox_cgroup_default_is_none() {
        let cfg = Config::default();
        assert!(
            cfg.sandbox_cgroup.is_none(),
            "sandbox_cgroup must default to None (D.11.1 off-by-default), got {:?}",
            cfg.sandbox_cgroup
        );
    }

    /// Catalog §D.11.1: `MOAGAN_SANDBOX_CGROUP=enabled` flips the
    /// knob to `Some(CgroupLimits::default())` so operators can opt
    /// in without editing `config.toml`. Locked against the JSON /
    /// garbage tests below because they all mutate the same env var.
    #[test]
    fn config_env_var_cgroup_enabled_overrides_default() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_CGROUP", "enabled");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_CGROUP");
        }
        assert!(
            cfg.sandbox_cgroup.is_some(),
            "MOAGAN_SANDBOX_CGROUP=enabled must opt in, got {:?}",
            cfg.sandbox_cgroup
        );
        let limits = cfg.sandbox_cgroup.expect("Some after opt-in");
        assert_eq!(limits.cpu_max.as_deref(), Some("100000 100000"));
        assert_eq!(limits.memory_max_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(limits.pids_max, Some(512));
    }

    /// Catalog §D.11.1: truthy aliases (`1` / `true` / `yes` / `on`)
    /// all opt in. Pin the parser contract so a refactor that
    /// tightens it surfaces as a test failure.
    #[test]
    fn config_env_var_cgroup_truthy_aliases_opt_in() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["1", "true", "TRUE", "yes", "on"] {
            unsafe {
                std::env::set_var("MOAGAN_SANDBOX_CGROUP", value);
            }
            let mut cfg = Config::default();
            cfg.apply_env_overrides();
            assert!(
                cfg.sandbox_cgroup.is_some(),
                "MOAGAN_SANDBOX_CGROUP={value} must opt in, got {:?}",
                cfg.sandbox_cgroup
            );
        }
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_CGROUP");
        }
    }

    /// Catalog §D.11.1: the env var also accepts a JSON object so
    /// operators can scope the limits without editing `config.toml`.
    #[test]
    fn config_env_var_cgroup_json_overrides_default() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let json = r#"{"cpu_max":"50000 100000","memory_max_bytes":1073741824,"pids_max":64}"#;
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_CGROUP", json);
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_CGROUP");
        }
        let limits = cfg.sandbox_cgroup.expect("Some after JSON opt-in");
        assert_eq!(limits.cpu_max.as_deref(), Some("50000 100000"));
        assert_eq!(limits.memory_max_bytes, Some(1_073_741_824));
        assert_eq!(limits.pids_max, Some(64));
    }

    /// Catalog §D.11.1: garbage / whitespace env values are ignored
    /// so a stale / malformed export does not silently flip the
    /// default.
    #[test]
    fn config_env_var_cgroup_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_SANDBOX_CGROUP", "   ");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_SANDBOX_CGROUP");
        }
        assert!(
            cfg.sandbox_cgroup.is_none(),
            "garbage env must not flip the default None, got {:?}",
            cfg.sandbox_cgroup
        );
    }

    /// Catalog §D.11.1: TOML round-trip preserves `sandbox_cgroup`
    /// so operators can pin their choice in
    /// `~/.config/moagan/config.toml`.
    #[test]
    fn config_sandbox_cgroup_toml_round_trip() {
        let cfg = Config {
            sandbox_cgroup: Some(CgroupLimits {
                cpu_max: Some("25000 100000".into()),
                memory_max_bytes: Some(512 * 1024 * 1024),
                pids_max: Some(64),
            }),
            ..Config::default()
        };
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(
            back.sandbox_cgroup, cfg.sandbox_cgroup,
            "TOML round-trip must preserve sandbox_cgroup"
        );
    }

    /// Track E (catalog §D.19.6): `rate_limit_per_provider` defaults
    /// to an empty map so the default install is unaffected by the
    /// token-bucket wiring. Operators opt in by setting entries in
    /// `~/.config/moagan/config.toml` or by exporting
    /// `MOAGAN_RATE_LIMIT_<provider>=<capacity>:<refill_per_sec>`.
    #[test]
    fn config_rate_limit_per_provider_default_is_empty() {
        let cfg = Config::default();
        assert!(
            cfg.rate_limit_per_provider.is_empty(),
            "rate_limit_per_provider must default to empty (D.19.6 off-by-default), got {:?}",
            cfg.rate_limit_per_provider
        );
    }

    /// `MOAGAN_RATE_LIMIT_<provider>=<capacity>:<refill_per_sec>` opts
    /// the named provider into a token bucket. The provider name in
    /// the env var is uppercased by convention; the config stores it
    /// lowercased to match the `[providers]` table keys.
    #[test]
    fn config_env_var_rate_limit_per_provider_populates_map() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_RATE_LIMIT_MINIMAX", "30:5");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_RATE_LIMIT_MINIMAX");
        }
        let entry = cfg
            .rate_limit_per_provider
            .get("minimax")
            .expect("minimax rate-limit entry must be populated");
        assert_eq!(entry.capacity, 30);
        assert_eq!(entry.refill_per_sec, 5);
    }

    /// Garbage env values are ignored so a stale / malformed export
    /// does not corrupt the install.
    #[test]
    fn config_env_var_rate_limit_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_RATE_LIMIT_MOCK", "no-colon-here");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_RATE_LIMIT_MOCK");
        }
        assert!(
            cfg.rate_limit_per_provider.is_empty(),
            "garbage rate-limit env must not populate the map, got {:?}",
            cfg.rate_limit_per_provider
        );
    }

    /// `MOAGAN_RATE_LIMIT_ROLE_<role>=<capacity>:<refill_per_sec>` opts
    /// the named role into a role-scoped token bucket. The role name
    /// matches `Role::as_str()` (snake_case); the prefix is distinct
    /// from the per-provider `MOAGAN_RATE_LIMIT_<provider>` so the
    /// two maps never collide. Mirrors the per-provider env test
    /// but exercises the role-scoped branch.
    #[test]
    fn config_env_var_rate_limit_role_tag_must_not_crash_other_role() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Sanity: setting a per-provider env var with a role-style
        // suffix would otherwise be misread as a provider. The
        // `MOAGAN_RATE_LIMIT_<suffix>` loop explicitly skips
        // `ROLE_*` suffixes so the per-role map stays empty when
        // only the per-provider var is set.
        unsafe {
            std::env::set_var("MOAGAN_RATE_LIMIT_MINIMAX", "30:5");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_RATE_LIMIT_MINIMAX");
        }
        let provider = cfg
            .rate_limit_per_provider
            .get("minimax")
            .expect("minimax entry must be populated by per-provider env");
        assert_eq!(provider.capacity, 30);
        assert_eq!(provider.refill_per_sec, 5);
        assert!(
            cfg.rate_limit_per_role.is_empty(),
            "per-provider env var must not leak into the role map, got {:?}",
            cfg.rate_limit_per_role
        );
    }

    /// TOML round-trip preserves the per-provider rate-limit knobs
    /// so operators can pin their choice in
    /// `~/.config/moagan/config.toml`.
    #[test]
    fn config_rate_limit_per_provider_toml_round_trip() {
        let mut rate_limit_per_provider = std::collections::HashMap::new();
        rate_limit_per_provider.insert(
            "minimax".into(),
            RateLimitConfig {
                capacity: 20,
                refill_per_sec: 2,
                initial: None,
            },
        );
        let cfg = Config {
            rate_limit_per_provider,
            ..Config::default()
        };
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        let entry = back
            .rate_limit_per_provider
            .get("minimax")
            .expect("minimax entry must survive TOML round-trip");
        assert_eq!(entry.capacity, 20);
        assert_eq!(entry.refill_per_sec, 2);
    }

    /// Per-role rate-limit (catalog §D.19.6) defaults to an empty
    /// map so a fresh installation behaves bit-identical to a
    /// pre-fix run. The operator opts in via
    /// `[rate_limit_per_role]` in `~/.config/moagan/config.toml`.
    #[test]
    fn config_rate_limit_per_role_default_is_empty() {
        let cfg = Config::default();
        assert!(
            cfg.rate_limit_per_role.is_empty(),
            "rate_limit_per_role must default to empty (D.19.6 off-by-default), got {:?}",
            cfg.rate_limit_per_role
        );
    }

    /// Per-role rate-limit entries survive a TOML round-trip so
    /// operators can persist the per-role override across
    /// `moagan` invocations. The key is the `Role::as_str()`
    /// (snake_case); the value is the bucket shape.
    #[test]
    fn config_rate_limit_per_role_toml_round_trip() {
        let mut rate_limit_per_role = std::collections::HashMap::new();
        rate_limit_per_role.insert(
            "tagger".into(),
            RateLimitConfig {
                capacity: 30,
                refill_per_sec: 2,
                initial: Some(30),
            },
        );
        let cfg = Config {
            rate_limit_per_role,
            ..Config::default()
        };
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        let entry = back
            .rate_limit_per_role
            .get("tagger")
            .expect("tagger entry must survive TOML round-trip");
        assert_eq!(entry.capacity, 30);
        assert_eq!(entry.refill_per_sec, 2);
        assert_eq!(entry.initial, Some(30));
    }

    #[test]
    fn export_config_default_is_blake3() {
        // The default matches `CacheHashAlgo::default()` (in
        // `llm::wire`) and the canonical cache-key
        // implementation: BLAKE3 is the day-to-day internal
        // hash. SHA-256 stays available via
        // `--hash-algo sha256` for operators who want to
        // re-verify the sidecar with the usual CLI tooling.
        let cfg = Config::default();
        assert!(matches!(
            cfg.export.hash_algo,
            crate::cli::flags_batch::HashAlgo::Blake3
        ));
    }

    #[test]
    fn export_config_round_trips_through_toml() {
        use crate::cli::flags_batch::HashAlgo;
        let mut cfg = Config::default();
        cfg.export.hash_algo = HashAlgo::Sha256;
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert!(matches!(
            back.export.hash_algo,
            crate::cli::flags_batch::HashAlgo::Sha256
        ));
    }

    /// K.4: `MOAGAN_RESEARCH_API_KEY` populates
    /// `Config::research.api_key`. Empty / whitespace exports are
    /// ignored so a stale shell value cannot forge an empty
    /// Authorization header at fetch time.
    #[test]
    fn config_research_api_key_from_env() {
        let mut cfg = Config::default();
        assert!(cfg.research.api_key.is_none(), "default must be None");

        unsafe {
            std::env::set_var("MOAGAN_RESEARCH_API_KEY", "ghp_token_123");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_RESEARCH_API_KEY");
        }
        assert_eq!(
            cfg.research.api_key.as_deref(),
            Some("ghp_token_123"),
            "non-empty env var must populate research.api_key"
        );

        let mut cfg = Config::default();
        unsafe {
            std::env::set_var("MOAGAN_RESEARCH_API_KEY", "   ");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_RESEARCH_API_KEY");
        }
        assert!(
            cfg.research.api_key.is_none(),
            "whitespace env var must NOT populate research.api_key"
        );
    }

    #[test]
    fn config_per_host_rate_limit_from_env() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_RESEARCH_RATE_LIMIT_DOCS_RS", "3:7");
        }
        let mut config = Config::default();
        config.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_RESEARCH_RATE_LIMIT_DOCS_RS");
        }

        let rate_limit = config.research.per_host_rate_limit.get("docs.rs").unwrap();
        assert_eq!(rate_limit.capacity, 3);
        assert_eq!(rate_limit.refill_per_sec, 7);
        assert_eq!(rate_limit.initial, None);
    }

    /// Per-provider `omit_max_tokens` env override:
    /// `MOAGAN_<NAME>_OMIT_MAX_TOKENS=true` flips the flag for the
    /// named provider section. Dots and hyphens in the section name
    /// are rewritten to underscores, so `opencode` becomes
    /// `MOAGAN_OPENCODE_OMIT_MAX_TOKENS`. Other sections are
    /// untouched so the env var stays scoped. v0.10 update: the
    /// override targets the canonical section (`opencode`),
    /// not the v0.9 per-model alias `gpt-5.6-luna` (which is now
    /// a model on the opencode section's `models[]` list). Locked
    /// against the `_false_resets` and `_garbage_is_ignored` tests
    /// because they all touch the same env var.
    #[test]
    fn apply_env_overrides_sets_omit_max_tokens_per_provider() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Snapshot any pre-existing env value so the test is
        // independent of the operator's shell. Restore on the way
        // out, regardless of panics below.
        let prior = std::env::var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS").ok();
        unsafe {
            std::env::set_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS", "true");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS");
        }
        // Restore prior value if there was one.
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS", v);
            }
        }
        let oc = cfg
            .providers
            .get("opencode")
            .expect("opencode must be in default providers");
        assert!(
            oc.omit_max_tokens,
            "MOAGAN_OPENCODE_OMIT_MAX_TOKENS=true must opt in"
        );
        // Untouched section must remain `false`.
        let minimax = cfg
            .providers
            .get("minimax")
            .expect("minimax must be in default providers");
        assert!(
            !minimax.omit_max_tokens,
            "other sections must NOT inherit the env override"
        );
    }

    /// `MOAGAN_<NAME>_OMIT_MAX_TOKENS=false` on a TOML-true config
    /// resets the flag (the env override is the canonical mechanism
    /// to flip the default in either direction).
    #[test]
    fn apply_env_overrides_omit_max_tokens_false_resets() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS").ok();
        let mut cfg = Config::default();
        // Pretend the TOML flipped the bit on.
        if let Some(spec) = cfg.providers.get_mut("opencode") {
            spec.omit_max_tokens = true;
        }
        unsafe {
            std::env::set_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS", "false");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS");
        }
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS", v);
            }
        }
        let oc = cfg
            .providers
            .get("opencode")
            .expect("opencode must be in default providers");
        assert!(
            !oc.omit_max_tokens,
            "MOAGAN_OPENCODE_OMIT_MAX_TOKENS=false must opt out"
        );
    }

    /// Garbage / whitespace env values are ignored so a stale export
    /// does not silently flip the flag.
    #[test]
    fn apply_env_overrides_omit_max_tokens_garbage_is_ignored() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS").ok();
        unsafe {
            std::env::set_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS", "   ");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS");
        }
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("MOAGAN_OPENCODE_OMIT_MAX_TOKENS", v);
            }
        }
        let oc = cfg
            .providers
            .get("opencode")
            .expect("opencode must be in default providers");
        assert!(
            !oc.omit_max_tokens,
            "garbage env must not flip the default false, got {}",
            oc.omit_max_tokens
        );
    }

    /// `ProviderConfig::omit_max_tokens` survives a TOML round-trip so
    /// operators can pin their choice in `~/.config/moagan/config.toml`
    /// via `[providers.<name>]\nomit_max_tokens = true`. v0.10
    /// update: `gpt-5.6-luna` is a model on the canonical `opencode`
    /// section (the per-model alias sections were collapsed); the
    /// round-trip flips the flag on the opencode section and the
    /// assertion checks the model survives under the canonical name.
    #[test]
    fn provider_config_omit_max_tokens_toml_round_trip() {
        let mut cfg = Config::default();
        if let Some(spec) = cfg.providers.get_mut("opencode") {
            spec.omit_max_tokens = true;
        }
        let raw = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        let oc = back
            .providers
            .get("opencode")
            .expect("opencode section must survive TOML round-trip");
        assert!(
            oc.omit_max_tokens,
            "TOML round-trip must preserve omit_max_tokens"
        );
        // Pin the v0.10 canonical schema: gpt-5.6-luna is a model on
        // the opencode section, NOT a top-level section.
        assert!(
            oc.models.iter().any(|m| m.id == "gpt-5.6-luna"),
            "opencode section must carry gpt-5.6-luna on models[]"
        );
    }

    // ----------------------------------------------------------------
    // PR-B2: strict cwd-overrides-user config precedence.
    //
    // Each test acquires `TEST_CONFIG_LOCK` for the duration of its
    // env-var mutations and restores both cwd and HOME on Drop. The
    // tests run on the shared process-wide state so a leaked cwd
    // change would poison every subsequent test in the module.
    // ----------------------------------------------------------------

    /// A unique sentinel marker in a `moagan.toml` is the
    /// observable: if `Config::load` reads the file we wrote, the
    /// resulting `Config.providers` contains the provider with our
    /// marker. Otherwise (defaults / wrong file), the marker is
    /// absent. We use a provider name that cannot collide with the
    /// built-in defaults.
    fn write_marker_toml(dir: &std::path::Path, marker: &str) -> std::path::PathBuf {
        let body = format!(
            r#"
[providers.{marker}]
kind = "mock"
endpoint = "mock://{marker}"
model = "mock-{marker}"
"#
        );
        let p = dir.join("moagan.toml");
        std::fs::write(&p, body).expect("write cwd moagan.toml");
        p
    }

    #[test]
    fn cwd_moagan_toml_overrides_user_xdg() {
        let _lock = TEST_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _cwd_lock = crate::TEST_CWD_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let _cwd = CwdGuard::new();
        unsafe {
            std::env::remove_var("MOAGAN_CONFIG");
        }

        let cwd_dir = tempfile::tempdir().unwrap();
        _cwd.chdir(cwd_dir.path());

        // Build a fake user XDG layout: $HOME/.config/moagan/config.toml.
        let home_dir = tempfile::tempdir().unwrap();
        let xdg_dir = home_dir.path().join(".config").join("moagan");
        std::fs::create_dir_all(&xdg_dir).unwrap();
        let xdg_body = r#"
[providers.user_xdg_marker]
kind = "mock"
endpoint = "mock://user-xdg"
model = "mock-user-xdg"
"#;
        std::fs::write(xdg_dir.join("config.toml"), xdg_body).unwrap();
        unsafe {
            std::env::set_var("HOME", home_dir.path());
            std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config"));
        }

        // Write the cwd file with a DIFFERENT marker.
        write_marker_toml(cwd_dir.path(), "cwd_marker");

        let cfg = Config::load().expect("Config::load succeeds");
        assert!(
            cfg.providers.contains_key("cwd_marker"),
            "cwd moagan.toml must be loaded; providers: {:?}",
            cfg.providers.keys().collect::<Vec<_>>()
        );
        assert!(
            !cfg.providers.contains_key("user_xdg_marker"),
            "user XDG config.toml must be IGNORED when cwd file is present (strict precedence)"
        );
    }

    #[test]
    fn moagan_config_env_var_overrides_cwd_file() {
        let _lock = TEST_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _cwd_lock = crate::TEST_CWD_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let _cwd = CwdGuard::new();

        // Build three disjoint directories: cwd, env-var target,
        // and a fake HOME (so the XDG fallback does not collide).
        let cwd_dir = tempfile::tempdir().unwrap();
        let env_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        _cwd.chdir(cwd_dir.path());
        unsafe {
            std::env::set_var("HOME", home_dir.path());
            std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config"));
        }

        // Cwd file says "cwd_marker" — must be IGNORED.
        write_marker_toml(cwd_dir.path(), "cwd_marker");

        // Env-var file says "env_var_marker" — must WIN.
        let env_path = env_dir.path().join("env_var_config.toml");
        std::fs::write(
            &env_path,
            r#"
[providers.env_var_marker]
kind = "mock"
endpoint = "mock://env-var"
model = "mock-env-var"
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("MOAGAN_CONFIG", &env_path);
        }

        let cfg = Config::load().expect("Config::load succeeds");
        assert!(
            cfg.providers.contains_key("env_var_marker"),
            "MOAGAN_CONFIG must win over cwd file; providers: {:?}",
            cfg.providers.keys().collect::<Vec<_>>()
        );
        assert!(
            !cfg.providers.contains_key("cwd_marker"),
            "cwd file must be IGNORED when MOAGAN_CONFIG is set"
        );
    }

    #[test]
    fn user_xdg_used_when_no_cwd_file() {
        let _lock = TEST_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _cwd_lock = crate::TEST_CWD_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let _cwd = CwdGuard::new();
        unsafe {
            std::env::remove_var("MOAGAN_CONFIG");
        }

        let cwd_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let xdg_dir = home_dir.path().join(".config").join("moagan");
        std::fs::create_dir_all(&xdg_dir).unwrap();
        std::fs::write(
            xdg_dir.join("config.toml"),
            r#"
[providers.user_xdg_only_marker]
kind = "mock"
endpoint = "mock://user-xdg-only"
model = "mock-user-xdg-only"
"#,
        )
        .unwrap();
        _cwd.chdir(cwd_dir.path()); // cwd has no moagan.toml
        unsafe {
            std::env::set_var("HOME", home_dir.path());
            std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config"));
        }

        let cfg = Config::load().expect("Config::load succeeds");
        assert!(
            cfg.providers.contains_key("user_xdg_only_marker"),
            "user XDG config.toml must be loaded when no cwd file exists"
        );
    }

    #[test]
    fn hidden_dotfile_alt_name_is_consulted() {
        let _lock = TEST_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _cwd_lock = crate::TEST_CWD_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let _cwd = CwdGuard::new();
        unsafe {
            std::env::remove_var("MOAGAN_CONFIG");
        }

        let cwd_dir = tempfile::tempdir().unwrap();
        _cwd.chdir(cwd_dir.path());
        // The XDG fallback must NOT be touched when the cwd has
        // any moagan.toml variant, even the hidden one.
        let home_dir = tempfile::tempdir().unwrap();
        let xdg_dir = home_dir.path().join(".config").join("moagan");
        std::fs::create_dir_all(&xdg_dir).unwrap();
        std::fs::write(
            xdg_dir.join("config.toml"),
            r#"
[providers.user_xdg_dotfile_test]
kind = "mock"
endpoint = "mock://user-xdg"
model = "mock-user-xdg"
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("HOME", home_dir.path());
            std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config"));
        }
        // Write the hidden alt-name only.
        std::fs::write(
            cwd_dir.path().join(".moagan.toml"),
            r#"
[providers.hidden_dotfile_marker]
kind = "mock"
endpoint = "mock://hidden-dotfile"
model = "mock-hidden-dotfile"
"#,
        )
        .unwrap();

        let cfg = Config::load().expect("Config::load succeeds");
        assert!(
            cfg.providers.contains_key("hidden_dotfile_marker"),
            "./.moagan.toml must be honoured as an alt cwd file name"
        );
        assert!(
            !cfg.providers.contains_key("user_xdg_dotfile_test"),
            "user XDG must be IGNORED when the hidden cwd file exists"
        );
    }

    #[test]
    fn existing_xdg_only_setup_still_loads() {
        // Backward-compat: a setup that has ONLY the user XDG file
        // (the pre-PR-B2 default) and no cwd file must continue to
        // load identically. We assert that all default providers are
        // present and the user's provider is wired in.
        let _lock = TEST_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _cwd_lock = crate::TEST_CWD_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let _cwd = CwdGuard::new();
        unsafe {
            std::env::remove_var("MOAGAN_CONFIG");
        }

        let cwd_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let xdg_dir = home_dir.path().join(".config").join("moagan");
        std::fs::create_dir_all(&xdg_dir).unwrap();
        // Operator overrides ONLY the model's temperature on the
        // `minimax` entry; everything else is default.
        std::fs::write(
            xdg_dir.join("config.toml"),
            r#"
[providers.minimax]
temperature = 0.42
"#,
        )
        .unwrap();
        _cwd.chdir(cwd_dir.path()); // empty cwd
        unsafe {
            std::env::set_var("HOME", home_dir.path());
            std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config"));
        }

        let cfg = Config::load().expect("Config::load succeeds");
        assert_eq!(
            cfg.providers.get("minimax").unwrap().temperature,
            Some(0.42),
            "operator's temperature override must reach the config"
        );
        // Defaults still present.
        assert!(cfg.providers.contains_key("minimax"));
        assert!(cfg.providers.contains_key("mock"));
        assert!(cfg.providers.contains_key("deepseek"));
    }

    #[test]
    fn warn_unknown_provider_keys_does_not_panic() {
        // The helper walks the raw TOML and emits warnings for
        // `[providers.X]` tables whose only keys are unknown to
        // `ProviderConfig` (e.g. `api_key = "..."`). The test pins
        // the behaviour: it must not panic and must produce the
        // expected `tracing::warn!` line (we don't assert on the
        // log capture here — that's covered by the existing
        // tracing subscriber test rig).
        let _lock = TEST_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _cwd_lock = crate::TEST_CWD_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let _cwd = CwdGuard::new();
        unsafe {
            std::env::remove_var("MOAGAN_CONFIG");
        }

        let cwd_dir = tempfile::tempdir().unwrap();
        _cwd.chdir(cwd_dir.path());
        let home_dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", home_dir.path());
            std::env::set_var("XDG_CONFIG_HOME", home_dir.path().join(".config"));
        }
        std::fs::write(
            cwd_dir.path().join("moagan.toml"),
            r#"
[providers.minimax]
api_key = "this-belongs-in-api_keys.toml"
"#,
        )
        .unwrap();

        let cfg = Config::load().expect("Config::load succeeds despite unknown keys");
        // The unknown `api_key` field is silently dropped by serde
        // (its default behaviour for unknown keys); the rest of the
        // default provider entry is kept.
        assert!(
            cfg.providers.contains_key("minimax"),
            "minimax entry must survive even though the user only wrote unknown keys"
        );
    }

    /// PR-D1: the `[discovery_matrix]` block round-trips through
    /// TOML so an operator's per-provider temperature profiles
    /// survive a `Config::load()` ↔ `Config::dump()` cycle. The
    /// test parses a hand-written TOML string (the form the user
    /// would put in `~/.config/moagan/config.toml`) and asserts
    /// that the typed `discovery_matrix.temperature_profiles` map
    /// carries the values through. The CLI flag
    /// `--temperature-profile` wins on conflict — see
    /// `cli::discover::run` for the merge order.
    ///
    /// Provider model keys with dots (e.g. `mimo-v2.5`) must be
    /// quoted — TOML treats unquoted `mimo-v2.5` as a dotted
    /// path, not a single key.
    #[test]
    fn config_discovery_block_parses() {
        let toml = r#"
[discovery_matrix]
default_profile = { temperatures = [0.5], replicas_per_temperature = 2 }

[discovery_matrix.temperature_profiles."minimax-m3"]
temperatures = [0.0, 0.3, 0.7, 1.0]
replicas_per_temperature = 4

[discovery_matrix.temperature_profiles."mimo-v2.5"]
temperatures = [0.5]
replicas_per_temperature = 2
"#;
        let cfg: Config = toml::from_str(toml).expect("TOML must parse");
        let profiles = &cfg.discovery_matrix.temperature_profiles;
        assert!(
            profiles.contains_key("minimax-m3"),
            "minimax-m3 profile must survive the parse; got keys {:?}",
            profiles.keys().collect::<Vec<_>>()
        );
        let m3 = &profiles["minimax-m3"];
        assert_eq!(m3.temperatures, vec![0.0, 0.3, 0.7, 1.0]);
        assert_eq!(m3.replicas_per_temperature, 4);
        let mimo = &profiles["mimo-v2.5"];
        assert_eq!(mimo.temperatures, vec![0.5]);
        assert_eq!(mimo.replicas_per_temperature, 2);
        let default = cfg
            .discovery_matrix
            .default_profile
            .as_ref()
            .expect("default_profile must be Some when set in TOML");
        assert_eq!(default.temperatures, vec![0.5]);
        assert_eq!(default.replicas_per_temperature, 2);
    }
}
