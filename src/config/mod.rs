//! Configuration model and loader.
//!
//! Resolution order (highest priority first):
//! 1. CLI flags (wired in commit 10).
//! 2. `MOAGAN_*` environment variables (e.g. `MOAGAN_MAX_PARALLELISM`).
//! 3. `~/.config/moagan/config.toml` if present.
//! 4. Built-in defaults.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
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
    /// Default provider name when `--provider` is omitted.
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

/// Track E (E8 partial): knobs for the two D.7.1 catalog roles
/// that the Discovery flow can opt-into invoke. The fields here
/// gate the helpers in `src/discovery/persona_angle.rs`; neither
/// role is auto-invoked today (the integration into
/// `DiscoveryCoordinator::run` is a separate PR).
///
/// All fields default to "off / no-op" so existing runs are
/// bit-identical when the section is absent from `config.toml`.
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
}

impl Default for DiscoveryWiringConfig {
    fn default() -> Self {
        Self {
            persona_enabled: false,
            angle_enabled: false,
            angle_clusters_min: 2,
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
        if total > 0.0 {
            (sum * weighted_avg + self.overall * overall) / total
        } else {
            0.0
        }
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
            default_provider: "minimax".to_owned(),
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
            discovery: DiscoveryWiringConfig::default(),
            export: ExportConfig::default(),
            selection_plan: default_selection_plan(),
        }
    }
}

fn default_providers() -> BTreeMap<String, ProviderConfig> {
    let mut m = BTreeMap::new();
    let make_minimax = |model: &str| ProviderConfig {
        kind: "minimax".to_owned(),
        endpoint: "https://api.minimax.io/anthropic/v1".to_owned(),
        model: model.to_owned(),
        max_tokens: Some(131072),
        temperature: Some(0.6),
        top_p: Some(0.95),
        hard_incompatibilities: vec!["anthropic-sdk".to_owned(), "claude-sdk".to_owned()],
    };
    m.insert("minimax".to_owned(), make_minimax("MiniMax-M3"));
    m.insert("minimax-m3".to_owned(), make_minimax("MiniMax-M3"));
    m.insert("minimax-m2.7".to_owned(), make_minimax("MiniMax-M2.7"));
    m.insert(
        "minimax-m2.7-highspeed".to_owned(),
        make_minimax("MiniMax-M2.7-highspeed"),
    );
    m.insert("minimax-m2.5".to_owned(), make_minimax("MiniMax-M2.5"));
    let make_deepseek = |model: &str| ProviderConfig {
        kind: "deepseek".to_owned(),
        endpoint: "https://api.deepseek.com/v1".to_owned(),
        model: model.to_owned(),
        max_tokens: Some(8192),
        temperature: Some(0.6),
        top_p: Some(0.95),
        hard_incompatibilities: vec![],
    };
    m.insert("deepseek".to_owned(), make_deepseek("deepseek-v4-flash"));
    // OpenCode Go models per the 2026-08-04 operator roster. The
    // dispatcher in `src/llm/opencode_go.rs` selects the right wire
    // format based on the model name (`endpoint_path_for`) and appends
    // the model-specific path to the stable base URL stored here. The
    // default temperature is 1.0 because the operator's primary kimi
    // family (kimi-k2.7-code) only accepts that value on this
    // subscription; per-model overrides live in
    // `MODEL_TEMPERATURE_OVERRIDES` for the rare model that requires a
    // different value.
    let make_opencode_go = |model: &str, endpoint: &str| ProviderConfig {
        kind: "opencode_go".to_owned(),
        endpoint: endpoint.to_owned(),
        model: model.to_owned(),
        max_tokens: Some(8192),
        temperature: Some(1.0),
        top_p: Some(0.95),
        hard_incompatibilities: vec![],
    };
    // All 18 OpenCode Go providers share the same base URL. The
    // dispatcher (`OpenCodeGoProvider::new`) appends the model-specific
    // path (`/v1/chat/completions`, `/v1/messages`, or `/v1/responses`)
    // at construction time via the concrete provider's URL builder.
    // Storing the base URL keeps the `Provider::endpoint()` contract
    // stable across the three wire formats.
    let oc_base = "https://opencode.ai/zen/go/v1";
    // `/v1/chat/completions` (OpenAI-compatible) — 10 models.
    m.insert(
        "opencode_go".to_owned(),
        make_opencode_go("kimi-k2.7-code", oc_base),
    );
    m.insert("kimi-k3".to_owned(), make_opencode_go("kimi-k3", oc_base));
    m.insert(
        "kimi-k2.6".to_owned(),
        make_opencode_go("kimi-k2.6", oc_base),
    );
    m.insert("glm-5.1".to_owned(), make_opencode_go("glm-5.1", oc_base));
    m.insert("glm-5.2".to_owned(), make_opencode_go("glm-5.2", oc_base));
    m.insert(
        "deepseek-v4-pro".to_owned(),
        make_opencode_go("deepseek-v4-pro", oc_base),
    );
    m.insert(
        "deepseek-v4-flash".to_owned(),
        make_opencode_go("deepseek-v4-flash", oc_base),
    );
    m.insert(
        "mimo-v2.5".to_owned(),
        make_opencode_go("mimo-v2.5", oc_base),
    );
    m.insert(
        "mimo-v2.5-pro".to_owned(),
        make_opencode_go("mimo-v2.5-pro", oc_base),
    );
    m.insert("hy3".to_owned(), make_opencode_go("hy3", oc_base));
    // `/v1/messages` (Anthropic-compatible) — 7 models.
    m.insert(
        "minimax-m3".to_owned(),
        make_opencode_go("minimax-m3", oc_base),
    );
    m.insert(
        "minimax-m2.7".to_owned(),
        make_opencode_go("minimax-m2.7", oc_base),
    );
    m.insert(
        "minimax-m2.5".to_owned(),
        make_opencode_go("minimax-m2.5", oc_base),
    );
    m.insert(
        "qwen3.8-max".to_owned(),
        make_opencode_go("qwen3.8-max", oc_base),
    );
    m.insert(
        "qwen3.7-max".to_owned(),
        make_opencode_go("qwen3.7-max", oc_base),
    );
    m.insert(
        "qwen3.7-plus".to_owned(),
        make_opencode_go("qwen3.7-plus", oc_base),
    );
    m.insert(
        "qwen3.6-plus".to_owned(),
        make_opencode_go("qwen3.6-plus", oc_base),
    );
    // `/v1/responses` (OpenAI Responses) — 1 model.
    m.insert(
        "gpt-5.6-luna".to_owned(),
        make_opencode_go("gpt-5.6-luna", oc_base),
    );
    m.insert(
        "mock".to_owned(),
        ProviderConfig {
            kind: "mock".to_owned(),
            endpoint: "mock://local".to_owned(),
            model: "mock-model".to_owned(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Provider implementation name (e.g. `"minimax"`, `"mock"`).
    pub kind: String,
    /// HTTP endpoint (Anthropic-compatible for kind `minimax`).
    pub endpoint: String,
    /// Default model name.
    pub model: String,
    /// Default max tokens per call.
    pub max_tokens: Option<u32>,
    /// Default sampling temperature.
    pub temperature: Option<f32>,
    /// Default top-p.
    pub top_p: Option<f32>,
    /// Crate names that must never appear in `Cargo.toml` together with
    /// this provider — runtime vs static guard.
    pub hard_incompatibilities: Vec<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "mock".to_owned(),
            endpoint: "mock://local".to_owned(),
            model: "mock-model".to_owned(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        }
    }
}

impl Config {
    /// Build the default configuration without touching the filesystem.
    pub fn defaults() -> Self {
        Self::default()
    }

    /// Load configuration from `~/.config/moagan/config.toml` if it
    /// exists, then apply `MOAGAN_*` env overrides. Returns defaults if
    /// no file is present.
    ///
    /// When the user's TOML overrides the `[providers]` table, we merge
    /// it with `default_providers()`: any provider in the user's TOML
    /// replaces the default with the same name; providers absent from
    /// the user's TOML keep their built-in defaults. This way adding a
    /// new default provider (Q6 deepseek, Q7 opencode-go, etc.) doesn't
    /// break existing operator configs that only override a subset.
    pub fn load() -> Result<Self> {
        let path = default_config_path();
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            toml::from_str(&raw).map_err(|e| {
                crate::Error::InvalidArgs(format!("config parse error at {path:?}: {e}"))
            })?
        } else {
            Self::default()
        };
        // Merge user's [providers] table with the defaults: user entries win.
        let defaults = default_providers();
        for (name, default_spec) in defaults {
            cfg.providers.entry(name).or_insert(default_spec);
        }
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// Load a domain-specific profile by name.
    ///
    /// Thin wrapper around [`Profile::load`] so callers can stay on
    /// `Config::load_profile(...)`. The profile is NOT applied to
    /// the live `Config` here — use [`Config::apply_profile`] when
    /// the caller wants the merges to take effect (e.g. after a
    /// `--profile <name>` CLI flag resolves).
    pub fn load_profile(name: &str) -> Result<Profile> {
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
        if profile.is_empty() {
            return;
        }
        let mut forbidden: Vec<String> = self.gate_forbidden_techs.clone();
        forbidden.extend(profile.gate_forbidden_techs.iter().cloned());
        forbidden.sort();
        forbidden.dedup();
        self.gate_forbidden_techs = forbidden;
        if let Some(v) = profile.gate_min_length {
            self.gate_min_length = v;
        }
        if let Some(v) = profile.gate_max_length {
            self.gate_max_length = v;
        }
        // Profile-defined temperature / judge-quorum overrides are
        // stored alongside the run config so any phase that wants
        // them can consult `Config::profile_*` without re-loading
        // the TOML. Future phases (per-role temperature wiring,
        // per-mode judge counts) read these maps directly.
        self.profile_temperature_overrides = profile.temperature_overrides.clone();
        self.profile_judge_quorum_overrides = profile.judge_quorum_overrides.clone();
    }

    /// Apply `MOAGAN_*` environment overrides. Any override that fails
    /// to parse is silently ignored; bad config is up to the user.
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("MOAGAN_MAX_PARALLELISM")
            && let Ok(n) = v.parse()
        {
            self.max_parallelism = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_SKETCH_TIMEOUT")
            && let Ok(n) = v.parse()
        {
            self.sketch_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_PHASE_TIMEOUT")
            && let Ok(n) = v.parse()
        {
            self.phase_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_TOTAL_TIMEOUT")
            && let Ok(n) = v.parse()
        {
            self.total_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_DEFAULT_PROVIDER") {
            self.default_provider = v;
        }
        if let Ok(v) = std::env::var("MOAGAN_MINIMAX_ENDPOINT")
            && !v.trim().is_empty()
        {
            for spec in self.providers.values_mut() {
                if spec.kind == "minimax" {
                    spec.endpoint = v.clone();
                }
            }
        }
        if let Ok(v) = std::env::var("MOAGAN_MINIMAX_MODEL")
            && !v.trim().is_empty()
        {
            for spec in self.providers.values_mut() {
                if spec.kind == "minimax" {
                    spec.model = v.clone();
                }
            }
        }
        if let Ok(v) = std::env::var("MOAGAN_JSON_REPAIR_V2_ENABLED") {
            let normalised = v.trim().to_ascii_lowercase();
            match normalised.as_str() {
                "true" | "1" | "yes" | "on" => self.llm.json_repair_v2_enabled = true,
                "false" | "0" | "no" | "off" => self.llm.json_repair_v2_enabled = false,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("MOAGAN_REPAIR_MAX_ROUNDS")
            && let Ok(n) = v.parse()
        {
            self.repair_max_rounds = n;
        }
        if let Ok(v) = std::env::var("MOAGAN_GATE_FORBIDDEN_TECHS")
            && !v.trim().is_empty()
        {
            self.gate_forbidden_techs = v.split(',').map(|s| s.trim().to_owned()).collect();
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
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_NETWORK_POLICY")
            && let Some(policy) = parse_network_policy_env(&v)
        {
            self.sandbox_network_policy = policy;
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_NAMESPACES")
            && let Ok(flags) = v.parse()
        {
            self.sandbox_namespaces = flags;
        }
        if let Ok(v) = std::env::var("MOAGAN_SANDBOX_SECCOMP")
            && let Some(kind) = parse_seccomp_policy_env(&v)
        {
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
        }
        // Track E (catalog §D.19.6): per-provider rate-limit knobs.
        // `MOAGAN_RATE_LIMIT_<provider>=<capacity>:<refill_per_sec>`
        // opts the named provider into the token bucket. Each entry
        // overwrites any previous value for the same provider so the
        // env var is the canonical last-write-wins surface. Garbage
        // values (missing colon, non-numeric tokens) are silently
        // ignored so a stale export does not corrupt an existing
        // TOML-loaded entry. The provider name is lowercased to
        // match the canonical `[providers]` table keys.
        for (key, value) in std::env::vars() {
            let Some(suffix) = key.strip_prefix("MOAGAN_RATE_LIMIT_") else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            let provider = suffix.to_ascii_lowercase();
            if let Some(cfg) = parse_rate_limit_env(&value) {
                self.rate_limit_per_provider.insert(provider, cfg);
            }
        }
    }
}

/// Parse the `MOAGAN_SANDBOX_CGROUP` env var (when it does not look
/// like a truthy flag) into a [`CgroupLimits`] profile. Returns
/// `None` for any value that does not parse; the caller is
/// expected to leave the existing knob alone in that case.
fn parse_cgroup_limits_env(s: &str) -> Option<CgroupLimits> {
    serde_json::from_str::<CgroupLimits>(s).ok()
}

/// Parse the `MOAGAN_RATE_LIMIT_<provider>` env var into a
/// [`RateLimitConfig`]. Accepts `<capacity>:<refill_per_sec>` (both
/// non-negative integers). Returns `None` for any value that does
/// not parse so a stale / malformed export leaves the existing knob
/// alone.
fn parse_rate_limit_env(s: &str) -> Option<RateLimitConfig> {
    let s = s.trim();
    let (cap_str, refill_str) = s.split_once(':')?;
    let capacity: u32 = cap_str.trim().parse().ok()?;
    let refill_per_sec: u32 = refill_str.trim().parse().ok()?;
    Some(RateLimitConfig {
        capacity,
        refill_per_sec,
        initial: None,
    })
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
    let s = s.trim();
    if s.eq_ignore_ascii_case("permissive") {
        return Some(SeccompPolicyKind::Permissive);
    }
    if s.eq_ignore_ascii_case("strict_rust_build") {
        return Some(SeccompPolicyKind::StrictRustBuild);
    }
    if let Ok(kind) = serde_json::from_str::<SeccompPolicyKind>(s) {
        return Some(kind);
    }
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
    let s = s.trim();
    if s.eq_ignore_ascii_case("off") {
        return Some(NetworkPolicy::Off);
    }
    if s.eq_ignore_ascii_case("open") {
        return Some(NetworkPolicy::Open);
    }
    if let Ok(list) = serde_json::from_str::<Vec<String>>(s) {
        return Some(NetworkPolicy::AllowList { hosts: list });
    }
    if let Ok(policy) = serde_json::from_str::<NetworkPolicy>(s) {
        return Some(policy);
    }
    if s.eq_ignore_ascii_case("allow_list") {
        return Some(NetworkPolicy::AllowList { hosts: Vec::new() });
    }
    None
}

impl Config {
    /// Resolve the configured provider by name. Returns
    /// `Error::InvalidArgs` if the provider is unknown.
    pub fn provider(&self, name: &str) -> Result<&ProviderConfig> {
        self.providers
            .get(name)
            .ok_or_else(|| crate::Error::InvalidArgs(format!("unknown provider: {name}")))
    }
}

fn default_config_path() -> PathBuf {
    if let Ok(env) = std::env::var("MOAGAN_CONFIG") {
        return PathBuf::from(env);
    }
    if let Some(proj) = directories::ProjectDirs::from("", "", "moagan") {
        return proj.config_dir().join("config.toml");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("moagan")
            .join("config.toml");
    }
    PathBuf::from("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.max_parallelism, 4);
        assert_eq!(cfg.sketch_timeout_secs, 120);
        assert_eq!(cfg.phase_timeout_secs, 0);
        assert_eq!(cfg.total_timeout_secs, 0);
        assert!(cfg.redact_in_telemetry);
        assert!(cfg.providers.contains_key("minimax"));
        assert!(cfg.providers.contains_key("minimax-m3"));
        assert!(cfg.providers.contains_key("minimax-m2.7"));
        assert!(cfg.providers.contains_key("minimax-m2.7-highspeed"));
        assert!(cfg.providers.contains_key("minimax-m2.5"));
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
        // Default config has the hardcoded production endpoint.
        let mut cfg = Config::default();
        let baseline = cfg.providers.get("minimax").unwrap().endpoint.clone();
        assert_eq!(baseline, "https://api.minimax.io/anthropic/v1");

        // With the env var set, apply_env_overrides rewrites every
        // provider whose kind is "minimax" but leaves other providers
        // (e.g. "mock") alone.
        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_ENDPOINT", "http://localhost:8086/x");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MINIMAX_ENDPOINT");
        }
        assert_eq!(
            cfg.providers.get("minimax").unwrap().endpoint,
            "http://localhost:8086/x"
        );
        assert_eq!(cfg.providers.get("mock").unwrap().endpoint, "mock://local");
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
        let mut cfg = Config::default();
        // Baseline: every direct-minimax provider carries its canonical model.
        assert_eq!(cfg.providers.get("minimax").unwrap().model, "MiniMax-M3");
        assert_eq!(
            cfg.providers.get("minimax-m2.7-highspeed").unwrap().model,
            "MiniMax-M2.7-highspeed"
        );

        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_MODEL", "MiniMax-M2.5");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MINIMAX_MODEL");
        }

        // Only direct-minimax providers (kind="minimax") reflect the env
        // value. The opencode_go entries (minimax-m3/m2.7/m2.5) are
        // routed through OpenCode Go and must NOT be touched by the
        // MOAGAN_MINIMAX_MODEL env override.
        for name in ["minimax", "minimax-m2.7-highspeed"] {
            assert_eq!(
                cfg.providers.get(name).unwrap().model,
                "MiniMax-M2.5",
                "provider {name} should pick up MOAGAN_MINIMAX_MODEL"
            );
        }
        for name in ["minimax-m3", "minimax-m2.7", "minimax-m2.5"] {
            assert_eq!(
                cfg.providers.get(name).unwrap().model,
                name,
                "opencode_go provider {name} must NOT pick up MOAGAN_MINIMAX_MODEL"
            );
        }
        // The mock provider must not be touched.
        assert_eq!(cfg.providers.get("mock").unwrap().model, "mock-model");
    }

    /// Empty / whitespace env values are ignored, so a stale export in
    /// the shell does not blank the configured model. Mirrors the
    /// `MOAGAN_MINIMAX_ENDPOINT` handling.
    #[test]
    fn env_overrides_minimax_model_ignores_blank() {
        let mut cfg = Config::default();
        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_MODEL", "   ");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MINIMAX_MODEL");
        }
        assert_eq!(cfg.providers.get("minimax").unwrap().model, "MiniMax-M3");
    }

    /// Q6 pin: DeepSeek is exposed with its canonical default model.
    #[test]
    fn default_providers_lists_deepseek() {
        let cfg = Config::default();
        let spec = cfg
            .providers
            .get("deepseek")
            .expect("deepseek missing from default providers");
        assert_eq!(spec.kind, "deepseek");
        assert_eq!(spec.model, "deepseek-v4-flash");
        assert_eq!(spec.endpoint, "https://api.deepseek.com/v1");
    }

    /// Q7 pin: OpenCode Go is exposed with the operator-default
    /// non-MiniMax / non-Direct-DeepSeek model (kimi-k2.7-code).
    #[test]
    fn default_providers_lists_opencode_go() {
        let cfg = Config::default();
        let spec = cfg
            .providers
            .get("opencode_go")
            .expect("opencode_go missing from default providers");
        assert_eq!(spec.kind, "opencode_go");
        assert_eq!(spec.model, "kimi-k2.7-code");
        assert_eq!(spec.endpoint, "https://opencode.ai/zen/go/v1");
    }

    /// Q5 + 2026-08-04 pin: the canonical MiniMax models are exposed
    /// as separate provider entries under `kind="minimax"` (direct)
    /// or `kind="opencode_go"` (subscription, on the `/v1/messages`
    /// endpoint). The split matches the operator's 2026-08-04 model
    /// roster: minimax-m3/m2.7/m2.5 are routed through OpenCode Go.
    #[test]
    fn default_providers_lists_four_canonical_minimax_models() {
        let cfg = Config::default();
        let canonical = [
            // Direct MiniMax (kind="minimax")
            ("minimax", "minimax", "MiniMax-M3"),
            (
                "minimax-m2.7-highspeed",
                "minimax",
                "MiniMax-M2.7-highspeed",
            ),
            // OpenCode Go subscription (kind="opencode_go")
            ("minimax-m3", "opencode_go", "minimax-m3"),
            ("minimax-m2.7", "opencode_go", "minimax-m2.7"),
            ("minimax-m2.5", "opencode_go", "minimax-m2.5"),
        ];
        for (alias, kind, model) in canonical {
            let spec = cfg
                .providers
                .get(alias)
                .unwrap_or_else(|| panic!("alias {alias} missing from default providers"));
            assert_eq!(spec.kind, kind, "alias {alias} should map to {kind}");
            assert_eq!(
                spec.model, model,
                "alias {alias} should carry canonical model {model}"
            );
        }
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
}
