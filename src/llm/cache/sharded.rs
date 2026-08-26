//! LLM cache filesystem layout.
//!
//! The cache lives under `cache/llm/<root>/`. To avoid "directory
//! index full" / per-entry `stat` latency when a run produces a large
//! number of cache hits, entries are sharded by the first two hex
//! characters of the canonical BLAKE3 key. With a 64-hex key that
//! gives exactly 256 top-level directories, each named after a
//! specific shard prefix, e.g.:
//!
//! ```text
//! cache/llm/ab/abcdef0123...64hex....json
//! cache/llm/cd/0123456789...64hex....json
//! ...
//! ```
//!
//! Collisions are still possible in theory (two keys sharing the first
//! two hex chars), but the BLAKE3 hash has near-uniform bit
//! distribution, so in practice each shard contains roughly `N/256`
//! entries, where `N` is the total count of cached responses for the
//! run.
//!
//! This module is intentionally tiny: just enough layout math to keep
//! the rest of `cache::Cache` ignorant of where files live, so the
//! TTL / LRU follow-up commits do not have to touch file paths
//! again.

use std::path::{Path, PathBuf};

/// Number of hex characters used to bucket a key. Two hex chars == one
/// byte == 256 possible shards. Sixteen characters would already give
/// the full key, so anything above two only widens the directory
/// without improving the layout.
pub(super) const SHARD_HEX_LEN: usize = 2;

/// Return the shard directory name for `key`. The shard is the first
/// [`SHARD_HEX_LEN`] hex characters of `key`. Short keys (less than
/// [`SHARD_HEX_LEN`] hex chars) use the key itself as the shard, so an
/// empty key would land at the root — but the cache never mints
/// zero-length keys because `canonical_hash` always returns at least
/// one byte of hex.
pub(super) fn shard_for(key: &str) -> &str {
    let n = key.len().min(SHARD_HEX_LEN);
    tracing::trace!(
        key_len = key.len(),
        shard_len = n,
        "shard_for: computed shard prefix"
    );
    &key[..n]
}

/// Build the absolute path to the cache file for `key` under `root`.
///
/// Path layout: `<root>/<shard>/<key>.json` where `<shard>` is the
/// two-hex-char prefix of `key`. The `AtomicWriter` will lazily create
/// the shard directory on `store`, so call sites do not have to
/// `mkdir -p` themselves.
pub(super) fn path_for(root: &Path, key: &str) -> PathBuf {
    let path = root.join(shard_for(key)).join(format!("{key}.json"));
    tracing::trace!(path = %path.display(), "sharded: path_for built");
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_for_uses_first_two_hex_chars() {
        assert_eq!(shard_for("abcdef0123456789"), "ab");
        assert_eq!(shard_for("ff"), "ff");
    }

    #[test]
    fn shard_for_returns_key_verbatim_when_too_short() {
        // Defensive: cache_key must always be ≥ SHARD_HEX_LEN, but if a
        // degenerate key slips through we do not want to panic with an
        // out-of-bounds slice.
        assert_eq!(shard_for("a"), "a");
        assert_eq!(shard_for(""), "");
    }

    #[test]
    fn path_for_layout_matches_spec() {
        // canonical_hash returns 64 hex chars (BLAKE3 -> 32 bytes).
        let key = "abcdef0123456789".repeat(4);
        let path = path_for(std::path::Path::new("/tmp/cache"), &key);
        assert_eq!(
            path,
            std::path::PathBuf::from(format!("/tmp/cache/ab/{key}.json"))
        );
    }

    #[test]
    fn shard_distribution_is_uniform_across_256_buckets() {
        // Sanity check: when the input cycle walks every shard evenly,
        // each bucket must receive exactly the same number of
        // entries. This guards against future changes that break the
        // two-hex prefix by accident, and it also documents the
        // design intent: 256 top-level directories, ~N/256
        // entries each.
        let mut buckets = std::collections::HashMap::<String, usize>::new();
        // (i * 7) % 256 walks every shard four times across 1024
        // iterations (1024 == 4 * 256), so each bucket must collect
        // exactly 4 entries.
        for i in 0..1024u32 {
            let prefix = (i.wrapping_mul(7)) % 256;
            let key = format!("{prefix:02x}{:062x}", 0u64);
            *buckets.entry(shard_for(&key).to_owned()).or_default() += 1;
        }
        assert_eq!(buckets.len(), 256);
        let max = buckets.values().copied().max().unwrap();
        let min = buckets.values().copied().min().unwrap();
        assert_eq!(max, 4, "max={max}, min={min}");
        assert_eq!(min, 4, "min should be 4 for a 4x uniform cycle");
    }
}
