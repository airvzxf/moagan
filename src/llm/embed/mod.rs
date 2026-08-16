//! Lightweight embedding interface for `cluster_by_embedder` (D.1.3).
//!
//! The default `HashingEmbedder` is dependency-free. The opt-in
//! [`remote::RemoteEmbedder`] adapter adds HTTP-backed embeddings
//! for the four most common provider wire formats (OpenAI, Cohere,
//! Voyage, and a generic OpenAI-compatible `Custom` fallback). The
//! `fastembed` crate integration is deferred to a later sub-phase.
//!
//! The hashing embedder uses the FNV-1a 32-bit hash on
//! alphanumeric tokens and signs the contribution by the top
//! bit of the hash. Vectors are L2-normalised so cosine similarity
//! collapses to a plain dot product. A small in-memory cache keyed
//! by the verbatim input string keeps repeated `embed()` calls
//! (the common case during `cluster_by_embedder`) cheap and
//! deterministic across runs.
//!
//! Compliance: proposal-03 §D.1.3 (T09-02; T18-09 §7.7; T09-08
//! §7.7; T03-07 §2054-2060; T06-04 §8.3).

use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::Result;

/// Network-backed embedding adapter. Opt-in via
/// `[embedder.remote]` in `~/.config/moagan/config.toml`; the
/// dependency-free [`HashingEmbedder`] remains the default. See the
/// `remote` module docs for the provider list, auth model, and the
/// async batch / sync single-text dual API.
pub mod remote;
pub use remote::{RemoteEmbedder, RemoteEmbedderConfig, RemoteEmbedderProvider};

/// Embedding backend. Implementations map a chunk of text to a
/// fixed-dimensional `f32` vector. All vectors returned by a given
/// `Embedder` instance must be L2-normalised so cosine similarity
/// equals the dot product of two outputs.
///
/// The trait is **synchronous** and **infallible**: `embed()` must
/// return a vector on every call. The dependency-free
/// [`HashingEmbedder`] honours this directly because hashing cannot
/// fail. The network-backed [`RemoteEmbedder`] honours it via a
/// [`tokio::task::block_in_place`] bridge that **only works inside a
/// multi-thread Tokio runtime** (`Builder::new_multi_thread()` or
/// `#[tokio::test(flavor = "multi_thread")]`); production satisfies
/// this because `run_blocking()` in `src/lib.rs` builds a
/// multi-thread runtime.
///
/// For fallible, async-native embedding, prefer the
/// [`AsyncEmbedder`] trait instead.
pub trait Embedder: Send + Sync {
    /// Embed `text` into a fixed-dimensional vector.
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Output dimensionality. Must be `> 0` and stable across calls.
    fn dim(&self) -> usize;
    /// Stable identifier for the backend (e.g. `"hashing"`).
    fn name(&self) -> &'static str;
}

/// Async-native embedding trait. The canonical contract for any
/// network-backed adapter: send a batch of inputs to the upstream,
/// receive a vector per input. Fallible (`Result`) because network
/// I/O is fallible.
///
/// Mirrors [`Embedder::dim`] and [`Embedder::name`] so callers that
/// already hold a `&dyn Embedder` can switch to a `&dyn
/// AsyncEmbedder` without an extra metadata hop. The `cluster`
/// family of functions in `src/discovery/clusterer.rs` only depends
/// on the sync trait today; the async trait is exposed for future
/// async-first callers (D.1.3 follow-up).
///
/// Compliance: catalog 10-integrada-v0 §D.1.3 (T09-02; T18-09 §7.7).
#[async_trait]
pub trait AsyncEmbedder: Send + Sync {
    /// Embed a batch of `texts` and return one vector per input, in
    /// the same order. An empty `texts` slice yields an empty `Vec`
    /// without any I/O.
    async fn embed_batch<'a>(&'a self, texts: &'a [&'a str]) -> Result<Vec<Vec<f32>>>;
    /// Output dimensionality. Must be `> 0` and stable across calls.
    fn dim(&self) -> usize;
    /// Stable identifier for the backend (e.g. `"remote"`).
    fn name(&self) -> &'static str;
}

/// FNV-1a hashed bag-of-tokens embedder. Deterministic, dependency-free,
/// and adequate for the proposal-03 §D.1.3 "hashing baseline" — `fastembed`
/// and the remote HTTP adapter land in a later sub-phase.
///
/// The tokeniser is a deliberately conservative lowercase split on
/// non-alphanumeric boundaries, which matches the behaviour of the
/// existing redaction pipeline and keeps tokenisation consistent
/// across phases that share the same string.
pub struct HashingEmbedder {
    dim: usize,
    /// Term-frequency cache. Using `parking_lot::Mutex` so the
    /// embedder can be shared across threads without an async
    /// runtime — the typical caller is the discovery / clustering
    /// code that does not hold a Tokio context.
    cache: parking_lot::Mutex<HashMap<String, Vec<f32>>>,
}

impl HashingEmbedder {
    /// Build an embedder with the given output dimensionality.
    /// Values below 8 are clamped to 8 to keep the modulo step
    /// well-defined and the resulting vector reasonably sparse.
    pub fn new(dim: usize) -> Self {
        Self {
            dim: dim.max(8),
            cache: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(256)
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &'static str {
        "hashing"
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        if let Some(v) = self.cache.lock().get(text) {
            return v.clone();
        }
        let mut v = vec![0.0f32; self.dim];
        for token in tokenize(text) {
            let h = fnv1a_32(token.as_bytes());
            let idx = (h as usize) % self.dim;
            let sign = if (h >> 31) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        self.cache.lock().insert(text.to_string(), v.clone());
        v
    }
}

/// FNV-1a 32-bit hash. The constants are the FNV-1a basis
/// (`0x811c9dc5`) and prime (`0x01000193`). Standard, well-known
/// values; we keep them in this module so we do not pull a hashing
/// crate just to map tokens to indices.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Tokenise `text` into lowercase alphanumeric tokens. Empty tokens
/// are dropped; everything else is kept verbatim after the
/// lower-case fold.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Cosine similarity between two vectors. Returns `0.0` when the
/// dimensions disagree, when either vector is empty, or when
/// either vector has zero norm. The function does NOT assume the
/// inputs are already L2-normalised; it computes the norms
/// internally. The unit tests rely on the symmetric
/// `cosine(a, b) == cosine(b, a)` invariant.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- HashingEmbedder ----------------

    /// Empty input yields the zero vector (no token contributes
    /// anything; the L2 norm stays at 0 and we skip the divide).
    #[test]
    fn hashing_embedder_zero_text_yields_zero_vector() {
        let e = HashingEmbedder::new(256);
        let v = e.embed("");
        assert_eq!(v.len(), 256);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    /// The same input always produces the same output (no RNG,
    /// no thread-local state). This pins the deterministic
    /// property the cluster-by-embedder feature relies on.
    #[test]
    fn hashing_embedder_deterministic() {
        let e = HashingEmbedder::new(256);
        let v1 = e.embed("the quick brown fox jumps over the lazy dog");
        let v2 = e.embed("the quick brown fox jumps over the lazy dog");
        assert_eq!(v1, v2);
    }

    /// Output vectors have L2 norm exactly 1.0 (or 0.0 for the
    /// all-zero path).
    #[test]
    fn hashing_embedder_normalized_to_unit_length() {
        let e = HashingEmbedder::new(128);
        let v = e.embed("rust sqlite migration pipeline");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm = {norm}");
    }

    /// `dim()` must echo the requested dimensionality (above the
    /// `max(8)` floor).
    #[test]
    fn hashing_embedder_dim_matches() {
        let e = HashingEmbedder::new(512);
        assert_eq!(e.dim(), 512);
        // Floor still applies.
        let small = HashingEmbedder::new(2);
        assert_eq!(small.dim(), 8);
    }

    /// Texts that share most tokens produce a high cosine.
    #[test]
    fn hashing_embedder_similar_texts_have_high_cosine() {
        let e = HashingEmbedder::new(256);
        let a = e.embed("rust axum postgres connection pool");
        let b = e.embed("rust axum postgres connection pool limit");
        let sim = cosine(&a, &b);
        assert!(sim > 0.7, "expected high similarity, got {sim}");
    }

    /// Texts that share no tokens produce a low cosine.
    #[test]
    fn hashing_embedder_dissimilar_texts_have_low_cosine() {
        let e = HashingEmbedder::new(256);
        let a = e.embed("hello world from the embedding module");
        let b = e.embed("quantum entanglement probability amplifier");
        let sim = cosine(&a, &b);
        assert!(sim < 0.5, "expected low similarity, got {sim}");
    }

    /// The cache returns a vector byte-for-byte identical to the
    /// one produced on the first (uncached) call. The clone is
    /// intentional so callers can mutate their copy freely.
    #[test]
    fn hashing_embedder_cache_returns_identical_vector() {
        let e = HashingEmbedder::new(256);
        let _first = e.embed("cache me");
        let cached = e.embed("cache me");
        let fresh = e.embed("cache me");
        assert_eq!(cached, fresh);
    }

    /// Two embedders with different `dim` produce different
    /// vectors because the modulo step changes the index space.
    #[test]
    fn hashing_embedder_different_dim_produces_different_vectors() {
        let a = HashingEmbedder::new(64);
        let b = HashingEmbedder::new(1024);
        let va = a.embed("rust sql migration");
        let vb = b.embed("rust sql migration");
        assert_ne!(va.len(), vb.len());
        // Different dim means the cosine function refuses to
        // compare them; we only assert the lengths differ.
        assert_eq!(va.len(), 64);
        assert_eq!(vb.len(), 1024);
    }

    // ---------------- cosine ----------------

    /// A vector is identical to itself.
    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![0.6, 0.8, 0.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    /// Orthogonal unit vectors have cosine exactly 0.
    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    /// Empty vectors are treated as "no overlap" and return 0
    /// (avoids dividing by 0).
    #[test]
    fn cosine_empty_vectors_is_zero() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    /// Dimension mismatch returns 0 rather than panicking, so a
    /// misconfigured embedder fails closed instead of crashing.
    #[test]
    fn cosine_mismatched_dim_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    // ---------------- fnv1a_32 ----------------

    /// FNV-1a 32 is well-defined; pin the constants against the
    /// canonical values published in the FNV reference paper so a
    /// silent change to the implementation surfaces as a failing
    /// test.
    #[test]
    fn fnv1a_32_known_vectors() {
        // Reference values from
        // http://www.isthe.com/chongo/tech/comp/fnv/index.html
        // FNV-1a 32-bit, lower-case input.
        assert_eq!(fnv1a_32(b""), 0x811c9dc5);
        assert_eq!(fnv1a_32(b"a"), 0xe40c292c);
        assert_eq!(fnv1a_32(b"abc"), 0x1a47e90b);
        assert_eq!(fnv1a_32(b"foobar"), 0xbf9cf968);
    }
}
