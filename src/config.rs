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
    }

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
}
