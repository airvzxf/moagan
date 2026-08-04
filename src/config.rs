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
            repair_max_rounds: 5,
            gate_forbidden_techs: Vec::new(),
            gate_min_length: 50,
            gate_max_length: 5000,
            stability: StabilityConfig::default(),
            server: ServerConfig::default(),
            retention: RetentionConfig::default(),
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
    let make_opencode_go = |model: &str| ProviderConfig {
        kind: "opencode_go".to_owned(),
        endpoint: "https://opencode.ai/zen/go/v1".to_owned(),
        model: model.to_owned(),
        max_tokens: Some(8192),
        temperature: Some(1.0),
        top_p: Some(0.95),
        hard_incompatibilities: vec![],
    };
    m.insert("opencode_go".to_owned(), make_opencode_go("kimi-k2.7-code"));
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
    }
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
        // Baseline: every minimax provider carries its canonical model.
        assert_eq!(cfg.providers.get("minimax").unwrap().model, "MiniMax-M3");
        assert_eq!(
            cfg.providers.get("minimax-m2.7").unwrap().model,
            "MiniMax-M2.7"
        );

        unsafe {
            std::env::set_var("MOAGAN_MINIMAX_MODEL", "MiniMax-M2.5");
        }
        cfg.apply_env_overrides();
        unsafe {
            std::env::remove_var("MOAGAN_MINIMAX_MODEL");
        }

        // Every minimax provider reflects the env value.
        for name in [
            "minimax",
            "minimax-m3",
            "minimax-m2.7",
            "minimax-m2.7-highspeed",
            "minimax-m2.5",
        ] {
            assert_eq!(
                cfg.providers.get(name).unwrap().model,
                "MiniMax-M2.5",
                "provider {name} should pick up MOAGAN_MINIMAX_MODEL"
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

    /// Q5 pin: the 4 canonical MiniMax models must all be exposed as
    /// separate provider entries in `default_providers()`. This is the
    /// contract the smoke script depends on
    /// (`--provider minimax-m2.5` is a recognised alias of
    /// `MiniMax-M2.5`).
    #[test]
    fn default_providers_lists_four_canonical_minimax_models() {
        let cfg = Config::default();
        let canonical = [
            ("minimax", "MiniMax-M3"),
            ("minimax-m3", "MiniMax-M3"),
            ("minimax-m2.7", "MiniMax-M2.7"),
            ("minimax-m2.7-highspeed", "MiniMax-M2.7-highspeed"),
            ("minimax-m2.5", "MiniMax-M2.5"),
        ];
        for (alias, model) in canonical {
            let spec = cfg
                .providers
                .get(alias)
                .unwrap_or_else(|| panic!("alias {alias} missing from default providers"));
            assert_eq!(spec.kind, "minimax", "alias {alias} should map to minimax");
            assert_eq!(
                spec.model, model,
                "alias {alias} should carry canonical model {model}"
            );
        }
    }
}
