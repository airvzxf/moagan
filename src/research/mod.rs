//! External research fetcher (K.4 / proposal-04 §4).
//!
//! Narrower than the full proposal: 4 allowlist hosts only
//! ([`docs.rs`](allowlist::ALLOWED_HOSTS), `crates.io`,
//! `api.github.com`, `github.com`), [`fetcher::MAX_URLS_PER_CALL`]
//! URLs per call, [`fetcher::MAX_BYTES_PER_URL`] bytes per
//! response. Designed to ground proposals in current docs without
//! bloating token budget.
//!
//! Wire-up to the Sketch phase lands in a follow-up PR; the
//! intended slot is the `${known_apis}` placeholder so called
//! snippets augment the prompt rather than replace it.

pub mod allowlist;
pub mod fetcher;

pub use allowlist::ALLOWED_HOSTS;
pub use fetcher::{FetchError, MAX_BYTES_PER_URL, MAX_URLS_PER_CALL, ResearchSnippet, fetch_all};
