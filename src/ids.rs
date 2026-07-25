//! Identifiers and hashing helpers.
//!
//! `RunId` is a UUID v7 (time-ordered) per T01-06 §3.1 and V4 §1.4.
//!
//! Hashing: BLAKE3 is the day-to-day internal hash (catalog 10-integrada-v0
//! §D.6.1, Day 1, ~5–10x faster than SHA-256 on hot paths). SHA-256 is
//! retained for human-visible exports (`manifest.json` checksums, export
//! sidecars) so external auditors can verify with the usual tooling.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Newtype wrapping a UUID v7 used as the run identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub Uuid);

impl RunId {
    /// Mint a fresh time-ordered run id.
    pub fn new() -> Self {
        // uuid v7 is enabled by the v7 feature.
        Self(Uuid::now_v7())
    }

    /// Build a `RunId` from an existing UUID (e.g. loaded from disk).
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// 8-char short hex used for log prefixes. Not unique on its own.
    pub fn short(&self) -> String {
        self.0.simple().to_string()[..8].to_owned()
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for RunId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// BLAKE3 hex digest (lowercase). Day 1 internal hash.
pub fn blake3_hex(data: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(data);
    hex::encode(hasher.finalize().as_bytes())
}

/// SHA-256 hex digest (lowercase). Kept for export sidecars.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// Concatenate `parts` with a zero byte separator that cannot appear in
/// UTF-8 text. Used for cache key construction.
fn canonical_join(parts: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            buf.push(0x1f);
        }
        buf.extend_from_slice(p.as_bytes());
    }
    buf
}

/// Hash the canonical concatenation of all inputs. Used for cache keys
/// and idempotent call lookups.
pub fn canonical_hash(parts: &[&str]) -> String {
    let bytes = canonical_join(parts);
    blake3_hex(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_is_unique_and_ordered() {
        let a = RunId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = RunId::new();
        assert_ne!(a, b);
        assert!(a < b, "v7 ids must be time-ordered");
    }

    #[test]
    fn run_id_round_trip_string() {
        let id = RunId::new();
        let s = id.to_string();
        let back: RunId = s.parse().unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn run_id_short_is_8_hex() {
        let id = RunId::new();
        let s = id.short();
        assert_eq!(s.len(), 8);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn blake3_known_vector() {
        // BLAKE3("abc") = 6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85
        let h = blake3_hex(b"abc");
        assert_eq!(
            h,
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn sha256_known_vector() {
        let h = sha256_hex(b"abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn canonical_hash_differs_by_input() {
        let a = canonical_hash(&["role:intake", "phase:intake", "provider:mock"]);
        let b = canonical_hash(&["role:intake", "phase:intake", "provider:minimax"]);
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_hash_is_separator_safe() {
        // The separator must not collide with legitimate content.
        let a = canonical_hash(&["a", "bc"]);
        let b = canonical_hash(&["ab", "c"]);
        assert_ne!(a, b);
    }
}
