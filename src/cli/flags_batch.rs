//! CLI flags batch (catalog §D.14.6-.21, §D.15.2-.6).
//!
//! Implementation strategy: each flag is a free function with
//! its own env var fallback. Flags are NOT added to Config or
//! RunOptions to avoid breaking the 20+ literal constructors in
//! tests. Each flag is documented separately.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Hash algorithm supported by `--hash-algo` (D.14.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

/// D.15.5: validate that a `--max-parallelism` value does not
/// exceed the hard cap of 64 simultaneous LLM calls.
pub fn validate_max_parallelism(n: usize) -> Result<(), String> {
    if n > 64 {
        Err(format!("--max-parallelism={n} exceeds maximum 64"))
    } else {
        Ok(())
    }
}

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
}
