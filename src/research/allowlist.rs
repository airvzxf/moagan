//! Hard-coded allowlist of trusted hosts for the external research fetcher.
//!
//! The list is intentionally tiny. Track K.4 (proposal-04 §4) is the
//! narrowest viable scope: only four well-known, high-signal sources of
//! Rust / crate / GitHub documentation. Adding a host here means new
//! trust + new audit surface, so each addition is its own PR.
//!

/// Per-host fetch policy. `auth_bearer = true` opts the host into
/// the [`ResearchFetcher`] `Authorization: Bearer <api_key>` flow
/// (K.4). Hosts without the flag are pulled unauthenticated over
/// plain HTTPS — `docs.rs` and `crates.io` are public CDNs that
/// actively rate-limit and 5xx when an Authorization header is
/// present without a known token, so the default is `false`.
#[derive(Debug, Clone, Copy)]
pub struct HostPolicy {
    /// Canonical lowercase hostname (matches against `Url::host_str`).
    pub host: &'static str,
    /// When `true`, the [`ResearchFetcher`] will attach an
    /// `Authorization: Bearer <api_key>` header to outbound
    /// requests, provided the configured [`crate::config::Config::research`]
    /// carries a non-empty `api_key`.
    pub auth_bearer: bool,
}

/// Hosts the bounded research fetcher is permitted to fetch from.
/// Tuple-order matches the order they were specced in proposal-04 §4.
/// `api.github.com` carries `auth_bearer = true` so the fetcher can
/// rate-limit-bump past GitHub's 60-req/h anonymous ceiling; the
/// API key comes from `Config::research.api_key` (env
/// `MOAGAN_RESEARCH_API_KEY`).
pub const HOSTS: &[HostPolicy] = &[
    HostPolicy {
        host: "docs.rs",
        auth_bearer: false,
    },
    HostPolicy {
        host: "crates.io",
        auth_bearer: false,
    },
    HostPolicy {
        host: "api.github.com",
        auth_bearer: true,
    },
    HostPolicy {
        host: "github.com",
        auth_bearer: false,
    },
];

/// Hosts the bounded research fetcher is permitted to fetch from.
/// Tuple-order matches the order they were specced in proposal-04 §4.
/// Kept for backwards-compat callers (`research::ALLOWED_HOSTS` is
/// re-exported through [`crate::research::mod`]); the canonical
/// surface is now [`HOSTS`].
pub const ALLOWED_HOSTS: &[&str] = &["docs.rs", "crates.io", "api.github.com", "github.com"];

/// Case-insensitive membership test against [`ALLOWED_HOSTS`].
pub fn is_allowed(host: &str) -> bool {
    ALLOWED_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h))
}

/// Look up the host policy entry that matches `host` (case-insensitive
/// on the hostname). Returns `None` when the host is not in the
/// allowlist; the caller is expected to enforce the allowlist
/// separately via [`is_allowed`] before looking up policy details.
pub fn find_policy(host: &str) -> Option<&'static HostPolicy> {
    HOSTS
        .iter()
        .find(|p| p.host.eq_ignore_ascii_case(host))
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

    /// K.4: `api.github.com` is the single host opted into the
    /// `Authorization: Bearer <api_key>` path. Pin the contract so a
    /// refactor that flips the flag trips the test before it lands
    /// in production.
    #[test]
    fn auth_bearer_only_set_for_api_github_com() {
        assert!(find_policy("api.github.com").unwrap().auth_bearer);
        assert!(!find_policy("docs.rs").unwrap().auth_bearer);
        assert!(!find_policy("crates.io").unwrap().auth_bearer);
        assert!(!find_policy("github.com").unwrap().auth_bearer);
    }

    /// `find_policy` is case-insensitive on the hostname and
    /// returns `None` for unknown hosts.
    #[test]
    fn find_policy_matches_case_insensitive() {
        assert!(find_policy("API.GITHUB.COM").is_some());
        assert!(find_policy("Docs.RS").is_some());
        assert!(find_policy("evil.example.com").is_none());
    }
}
