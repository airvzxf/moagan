//! CLI flags batch (catalog §D.14.6-.21, §D.15.2-.6).
//!
//! Implementation strategy: each flag is a free function with
//! its own env var fallback. Flags are NOT added to Config or
//! RunOptions to avoid breaking the 20+ literal constructors in
//! tests. Each flag is documented separately.

use std::str::FromStr;

/// Hash algorithm supported by `--hash-algo` (D.14.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HashAlgo {
    /// SHA-256 (default).
    #[default]
    Sha256,
    /// BLAKE3.
    Blake3,
}

impl FromStr for HashAlgo {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "sha256" => Ok(Self::Sha256),
            "blake3" => Ok(Self::Blake3),
            other => Err(format!("invalid hash algo: {other}")),
        }
    }
}

/// Resolve the hash algorithm from `MOAGAN_HASH_ALGO` or fall
/// back to the supplied default. Invalid values silently fall
/// back (same convention as `MOAGAN_MINIMAX_ENDPOINT`).
pub fn hash_algo_from_env_or(default: HashAlgo) -> HashAlgo {
    std::env::var("MOAGAN_HASH_ALGO")
        .ok()
        .and_then(|s| HashAlgo::from_str(&s).ok())
        .unwrap_or(default)
}

/// D.14.7: returns `true` when the user supplied `-` so the
/// dispatcher knows to read the prompt from stdin instead of
/// using the literal string as the prompt.
pub fn prompt_is_stdin(prompt: &str) -> bool {
    prompt == "-"
}

/// Read the prompt body from stdin when `--prompt -` is set.
pub fn read_prompt_from_stdin() -> std::io::Result<String> {
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
    Ok(s)
}

/// D.14.9: name of the phase to resume a rerun from. Sourced
/// from `MOAGAN_CONTINUE_FROM_PHASE` so it works without a
/// subcommand-level flag.
pub fn continue_from_phase_from_env() -> Option<String> {
    std::env::var("MOAGAN_CONTINUE_FROM_PHASE").ok()
}

/// D.14.14: force a fresh evaluation pass even when cached
/// results exist. Reads `MOAGAN_FORCE_EVAL`.
pub fn force_eval_from_env() -> bool {
    std::env::var("MOAGAN_FORCE_EVAL")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// D.14.17: override proposal cardinality for the `batch` mode.
/// Reads `MOAGAN_BATCH_PROPOSALS`.
pub fn batch_proposals_from_env() -> Option<usize> {
    std::env::var("MOAGAN_BATCH_PROPOSALS")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// D.14.18: parse a budget suffix such as `1k`, `1.5M`, `2G`.
/// Returns the value in base units (bytes, USD cents, etc.).
pub fn parse_budget_suffix(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = if let Some(rest) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
        (rest, 1_000u64)
    } else if let Some(rest) = s.strip_suffix('m').or_else(|| s.strip_suffix('M')) {
        (rest, 1_000_000u64)
    } else if let Some(rest) = s.strip_suffix('g').or_else(|| s.strip_suffix('G')) {
        (rest, 1_000_000_000u64)
    } else {
        (s, 1u64)
    };
    num.parse::<f64>().ok().map(|n| (n * mult as f64) as u64)
}

/// D.14.19: `moagan telemetry cleanup --vacuum` analogue.
/// Reads `MOAGAN_TELEMETRY_VACUUM`.
pub fn vacuum_requested() -> bool {
    std::env::var("MOAGAN_TELEMETRY_VACUUM")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// D.14.21: `moagan inspect --json` analogue.
/// Reads `MOAGAN_INSPECT_JSON`.
pub fn inspect_json_from_env() -> bool {
    std::env::var("MOAGAN_INSPECT_JSON")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// D.15.5: validate that a `--max-parallelism` value does not
/// exceed the hard cap of 64 simultaneous LLM calls.
pub fn validate_max_parallelism(n: usize) -> Result<(), String> {
    if n > 64 {
        Err(format!("--max-parallelism={n} exceeds maximum 64"))
    } else {
        Ok(())
    }
}

/// Policy governing behaviour between batches in `batch` mode (D.15.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchPolicy {
    /// Continue on warnings; only stop on hard errors.
    #[default]
    Continue,
    /// Stop after the first failure.
    Stop,
    /// Require a human gate before continuing past the first failure.
    Gating,
}

/// D.15.2: routing TOML marker. (Real implementation is a future wire-up.)
pub const ROUTING_TOML_AVAILABLE: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_algo_from_str_parses_sha256() {
        let parsed: HashAlgo = "sha256".parse().expect("must parse sha256");
        assert_eq!(parsed, HashAlgo::Sha256);
    }

    #[test]
    fn hash_algo_from_str_parses_blake3() {
        let parsed: HashAlgo = "blake3".parse().expect("must parse blake3");
        assert_eq!(parsed, HashAlgo::Blake3);
    }

    #[test]
    fn hash_algo_from_str_rejects_invalid() {
        let err = "md5".parse::<HashAlgo>().expect_err("must error");
        assert!(err.contains("invalid hash algo"));
    }

    #[test]
    fn parse_budget_suffix_parses_k_m_g() {
        assert_eq!(parse_budget_suffix("1k"), Some(1_000));
        assert_eq!(parse_budget_suffix("1.5M"), Some(1_500_000));
        assert_eq!(parse_budget_suffix("2G"), Some(2_000_000_000));
        assert_eq!(parse_budget_suffix("256"), Some(256));
    }

    #[test]
    fn parse_budget_suffix_handles_pure_number() {
        assert_eq!(parse_budget_suffix("42"), Some(42));
        assert_eq!(parse_budget_suffix("  42  "), Some(42));
    }

    #[test]
    fn validate_max_parallelism_accepts_64() {
        assert!(validate_max_parallelism(64).is_ok());
        assert!(validate_max_parallelism(1).is_ok());
        assert!(validate_max_parallelism(0).is_ok());
    }

    #[test]
    fn validate_max_parallelism_rejects_65() {
        let err = validate_max_parallelism(65).expect_err("must error");
        assert!(err.contains("exceeds maximum 64"));
    }

    #[test]
    fn prompt_is_stdin_detects_dash() {
        assert!(prompt_is_stdin("-"));
        assert!(!prompt_is_stdin("hello"));
        assert!(!prompt_is_stdin(""));
    }

    #[test]
    fn batch_policy_default_is_continue() {
        assert_eq!(BatchPolicy::default(), BatchPolicy::Continue);
    }

    #[test]
    fn vacuum_requested_env_helper() {
        unsafe {
            std::env::remove_var("MOAGAN_TELEMETRY_VACUUM");
        }
        assert!(!vacuum_requested());
        unsafe {
            std::env::set_var("MOAGAN_TELEMETRY_VACUUM", "1");
        }
        assert!(vacuum_requested());
        unsafe {
            std::env::set_var("MOAGAN_TELEMETRY_VACUUM", "0");
        }
        assert!(!vacuum_requested());
        unsafe {
            std::env::remove_var("MOAGAN_TELEMETRY_VACUUM");
        }
    }
}
