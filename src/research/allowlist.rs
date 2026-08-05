//! Hard-coded allowlist of trusted hosts for the external research fetcher.
//!
//! The list is intentionally tiny. Track K.4 (proposal-04 §4) is the
//! narrowest viable scope: only four well-known, high-signal sources of
//! Rust / crate / GitHub documentation. Adding a host here means new
//! trust + new audit surface, so each addition is its own PR.

/// Hosts the bounded research fetcher is permitted to fetch from.
/// Tuple-order matches the order they were specced in proposal-04 §4.
pub const ALLOWED_HOSTS: &[&str] = &["docs.rs", "crates.io", "api.github.com", "github.com"];

/// Case-insensitive membership test against [`ALLOWED_HOSTS`].
pub fn is_allowed(host: &str) -> bool {
    ALLOWED_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_recognizes_docs_rs() {
        assert!(is_allowed("docs.rs"));
        assert!(is_allowed("DOCS.RS"));
    }

    #[test]
    fn allowlist_recognizes_crates_io() {
        assert!(is_allowed("crates.io"));
        assert!(is_allowed("CRATES.IO"));
    }

    #[test]
    fn allowlist_rejects_unknown_host() {
        assert!(!is_allowed("evil.example.com"));
        assert!(!is_allowed("localhost"));
        assert!(!is_allowed(""));
    }
}
