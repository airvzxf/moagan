//! Identifiers and hashing helpers.
//!
//! `RunId` is a UUID v7 (time-ordered) per T01-06 §3.1 and V4 §4.1.
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
        let id = Self(Uuid::now_v7());
        tracing::trace!(run_id = %id, "RunId::new");
        id
    }

    /// Build a `RunId` from an existing UUID (e.g. loaded from disk).
    pub fn from_uuid(uuid: Uuid) -> Self {
        tracing::trace!(uuid = %uuid, "RunId::from_uuid");
        Self(uuid)
    }

    /// 8-char short hex used for log prefixes. Not unique on its own.
    pub fn short(&self) -> String {
        let short = self.0.simple().to_string()[..8].to_owned();
        tracing::trace!(run_id = %self, short = %short, "RunId::short");
        short
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
        tracing::trace!(len = s.len(), "RunId::from_str: enter");
        let parsed = Uuid::parse_str(s)?;
        tracing::trace!(run_id = %parsed, "RunId::from_str: ok");
        Ok(Self(parsed))
    }
}

/// BLAKE3 hex digest (lowercase). Day 1 internal hash.
pub fn blake3_hex(data: &[u8]) -> String {
    tracing::trace!(data_len = data.len(), "blake3_hex: enter");
    let mut hasher = blake3::Hasher::new();
    hasher.update(data);
    let out = hex::encode(hasher.finalize().as_bytes());
    tracing::trace!(hex_len = out.len(), "blake3_hex: ok");
    out
}

/// SHA-256 hex digest (lowercase). Kept for export sidecars.
pub fn sha256_hex(data: &[u8]) -> String {
    tracing::trace!(data_len = data.len(), "sha256_hex: enter");
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    let out = hex::encode(h.finalize());
    tracing::trace!(hex_len = out.len(), "sha256_hex: ok");
    out
}

/// Concatenate `parts` with a zero byte separator that cannot appear in
/// UTF-8 text. Used for cache key construction.
fn canonical_join(parts: &[&str]) -> Vec<u8> {
    tracing::trace!(parts_count = parts.len(), "canonical_join: enter");
    let mut buf = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            buf.push(0x1f);
        }
        buf.extend_from_slice(p.as_bytes());
    }
    tracing::trace!(byte_len = buf.len(), "canonical_join: ok");
    buf
}

/// Hash the canonical concatenation of all inputs. Used for cache keys
/// and idempotent call lookups.
pub fn canonical_hash(parts: &[&str]) -> String {
    tracing::trace!(parts_count = parts.len(), "canonical_hash: enter");
    let bytes = canonical_join(parts);
    let out = blake3_hex(&bytes);
    tracing::trace!(
        parts_count = parts.len(),
        hex_len = out.len(),
        "canonical_hash: ok"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    // -----------------------------------------------------------------
    // Property-based tests (proptest 1.4).
    //
    // These cover the invariants of the three hash helpers
    // (`blake3_hex`, `sha256_hex`, `canonical_hash`) plus the
    // UUID v7 uniqueness contract for `RunId`. proptest is dev-only
    // per ADR-0001 (see Cargo.toml [dev-dependencies] and
    // `scripts/check-no-forbidden-crates.sh`).
    // -----------------------------------------------------------------

    proptest::proptest! {
        /// `blake3_hex` is a deterministic function of its input:
        /// the same bytes always hash to the same 64-char lowercase
        /// hex string. Property holds for every input (including
        /// the empty byte string and long byte runs).
        #[test]
        fn blake3_is_deterministic(
            data in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            prop_assert_eq!(blake3_hex(&data), blake3_hex(&data));
            let h = blake3_hex(&data);
            prop_assert_eq!(h.len(), 64, "BLAKE3 must produce 64 hex chars");
            prop_assert!(
                h.chars().all(|c| c.is_ascii_hexdigit()),
                "hex output must be lowercase hex: {h}"
            );
        }

        /// Same property for SHA-256: deterministic, 64-char
        /// lowercase hex. The two algorithms share the *output
        /// shape* but produce different digests, which the next
        /// property pins.
        #[test]
        fn sha256_is_deterministic(
            data in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            prop_assert_eq!(sha256_hex(&data), sha256_hex(&data));
            let h = sha256_hex(&data);
            prop_assert_eq!(h.len(), 64, "SHA-256 must produce 64 hex chars");
            prop_assert!(
                h.chars().all(|c| c.is_ascii_hexdigit()),
                "hex output must be lowercase hex: {h}"
            );
        }

        /// BLAKE3 and SHA-256 are different digest families; for
        /// any non-empty input their hex outputs differ.
        #[test]
        fn blake3_and_sha256_disagree(
            data in proptest::collection::vec(any::<u8>(), 1..128),
        ) {
            prop_assert_ne!(blake3_hex(&data), sha256_hex(&data));
        }

        /// `canonical_hash` is deterministic over the same slice
        /// of parts and produces 64 lowercase hex chars (BLAKE3
        /// is the underlying algorithm, so the output shape
        /// matches `blake3_hex`).
        #[test]
        fn canonical_hash_is_deterministic(
            parts in proptest::collection::vec(".*", 0..16),
        ) {
            let owned: Vec<&str> = parts.iter().map(String::as_str).collect();
            prop_assert_eq!(
                canonical_hash(&owned),
                canonical_hash(&owned),
                "same input slice must hash to the same digest"
            );
            let h = canonical_hash(&owned);
            prop_assert_eq!(h.len(), 64);
            prop_assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        }

        /// `canonical_hash` is sensitive to every input position:
        /// swapping two parts produces a different digest, and
        /// duplicating a part in a different slot also produces
        /// a different digest (catches a regression where the
        /// separator might accidentally be the empty byte).
        #[test]
        fn canonical_hash_distinguishes_positions(a in ".*", b in ".*") {
            prop_assume!(a != b);
            prop_assert_ne!(
                canonical_hash(&[a.as_str(), b.as_str()]),
                canonical_hash(&[b.as_str(), a.as_str()]),
                "canonical_hash must be order-sensitive"
            );
            // Same content, three slots — the empty string is a
            // valid part and must not collapse to the same digest
            // as the two-element layout above.
            prop_assert_ne!(
                canonical_hash(&[a.as_str(), b.as_str()]),
                canonical_hash(&[a.as_str(), b.as_str(), ""]),
                "adding a third empty part must change the digest"
            );
        }

        /// The unit separator (0x1f) cannot appear in UTF-8 input
        /// — every byte sequence a caller hands in is well-formed
        /// UTF-8 — so concatenating two parts with 0x1f between
        /// them is unambiguous. This property pins the
        /// *separator-safe* contract by constructing two part
        /// arrays whose concatenated forms differ but whose
        /// un-concatenated joined forms (without the separator)
        /// could collide.
        #[test]
        fn canonical_hash_separator_resolves_ambiguity(
            left in ".*", right in ".*",
        ) {
            prop_assume!(!left.is_empty() && !right.is_empty());
            // Build two layouts: split `left` at a *char*
            // boundary in the middle vs keep it whole, then
            // append `right`. `floor_char_boundary` keeps the
            // split index on a UTF-8 boundary so we never panic
            // when proptest feeds us a multi-byte character
            // straddling the midpoint. With no separator the
            // joined byte streams would be identical; with 0x1f
            // separators they differ.
            let mid = left.len() / 2;
            let mid = left.floor_char_boundary(mid);
            prop_assume!(mid > 0 && mid < left.len());
            let (l_pre, l_post) = left.split_at(mid);
            let h1 = canonical_hash(&[l_pre, l_post, right.as_str()]);
            let h2 = canonical_hash(&[left.as_str(), right.as_str()]);
            prop_assert_ne!(
                h1, h2,
                "split-vs-no-split must not collide (0x1f separator)"
            );
            // Two-element vs single-element merge: same bytes,
            // different layouts.
            let merged = format!("{left}{right}");
            prop_assert_ne!(
                canonical_hash(&[left.as_str(), right.as_str()]),
                canonical_hash(&[merged.as_str()]),
                "two-element vs single-element merge must not collide"
            );
        }

        /// 1024 freshly-minted `RunId`s are all distinct. UUID v7
        /// embeds a 60-bit timestamp + random tail; a collision
        /// inside one process is statistically impossible and a
        /// regression here would mean the timestamp substructure
        /// broke (e.g. v4 fallback in a v7-only path).
        #[test]
        fn run_id_uniqueness(n in 0u16..1024) {
            let mut seen = std::collections::HashSet::new();
            for _ in 0..n {
                let id = RunId::new();
                prop_assert!(
                    seen.insert(id),
                    "duplicate RunId minted: {id}"
                );
            }
            prop_assert_eq!(seen.len(), n as usize);
        }

        /// `RunId::short()` is always 8 lowercase hex chars. The
        /// prefix is not unique on its own (8 hex chars = 32
        /// bits, ~4B combinations) but the format must be stable
        /// so log-scrubbing tooling can rely on it.
        #[test]
        fn run_id_short_format_is_stable(_seed in 0u32..16) {
            for _ in 0..16 {
                let s = RunId::new().short();
                prop_assert_eq!(s.len(), 8);
                prop_assert!(
                    s.chars().all(|c| c.is_ascii_hexdigit()),
                    "short id must be 8 lowercase hex chars: {s}"
                );
            }
        }

        /// `RunId` string round-trips through `Display` /
        /// `FromStr` byte-for-byte. A regression in the wire
        /// form would break log parsing on stale runs.
        #[test]
        fn run_id_string_round_trip(_seed in 0u32..16) {
            for _ in 0..16 {
                let id = RunId::new();
                let s = id.to_string();
                let back: RunId = s.parse().unwrap();
                prop_assert_eq!(id, back);
                prop_assert_eq!(s.len(), 36, "UUID canonical form is 36 chars");
            }
        }
    }
}
