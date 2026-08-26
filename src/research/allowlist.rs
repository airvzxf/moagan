//! Hard-coded allowlist of trusted hosts for the external research fetcher.
//!
//! The list is intentionally tiny. Track K.4 (proposal-04 §4) is the
//! narrowest viable scope: only four well-known, high-signal sources of
//! Rust / crate / GitHub documentation. Adding a host here means new
//! trust + new audit surface, so each addition is its own PR.
//!

/// Per-host fetch policy. `auth_bearer = true` opts the host into
/// the [`ResearchFetcher`] `Authorization: Bearer <token>` flow
/// (K.4). Hosts without the flag are pulled unauthenticated over
/// plain HTTPS — `docs.rs` and `crates.io` are public CDNs that
/// actively rate-limit and 5xx when an Authorization header is
/// present without a known token, so the default is `false`.
///
/// K.4 sub-3 (per-host bearer): the optional `bearer_token_env`
/// field pins the *name* of the environment variable that holds
/// the bearer token for this host. The fetcher reads the env var
/// at request time and attaches `Authorization: Bearer <value>`
/// when (a) the operator has populated the env var AND (b) the
/// policy opt-in flag (`auth_bearer`) is set. An unset or empty
/// env var gracefully degrades to no-auth (backward-compat with
/// the v0.7.x single-token behaviour).
///
/// Operators can override the env var name via the
/// `[research.auth]` map in `~/.config/moagan/config.toml`
/// (see [`crate::config::ResearchAuthConfig`]). The override
/// wins over the static policy default so an operator pointing
/// at a custom secrets-manager convention does not have to
/// recompile.
#[derive(Debug, Clone, Copy)]
pub struct HostPolicy {
    /// Canonical lowercase hostname (matches against `Url::host_str`).
    pub host: &'static str,
    /// When `true`, the [`ResearchFetcher`] will attach an
    /// `Authorization: Bearer <token>` header to outbound
    /// requests, provided the per-host token resolution
    /// (static `bearer_token_env` + `[research.auth]` override)
    /// produces a non-empty value.
    pub auth_bearer: bool,
    /// Name of the environment variable that holds the bearer
    /// token for this host. `None` (the default for public
    /// CDNs) keeps the host unauthenticated.
    ///
    /// The fetcher consults this name at request time; an unset
    /// or empty env var does NOT raise an error — the request
    /// just goes out without the Authorization header. This
    /// preserves the v0.7.x behaviour where an operator without
    /// the `MOAGAN_RESEARCH_API_KEY` env var gets clean
    /// anonymous requests.
    pub bearer_token_env: Option<&'static str>,
}

/// Resolve the env var name that holds the bearer token for
/// `host`, applying the operator-supplied `[research.auth]`
/// override on top of the static [`HostPolicy::bearer_token_env`]
/// default. Returns `None` when neither source declares an env
/// var for the host. The override map is keyed by the canonical
/// hostname so `docs.rs` and `Docs.RS` map to the same entry.
///
/// Resolution order:
/// 1. `overrides.get(canonical_host)` (operator override).
/// 2. `find_policy(host)?.bearer_token_env` (static default).
/// 3. `None` (no auth on this host).
pub fn bearer_token_env_for<'a>(
    host: &str,
    overrides: &'a std::collections::HashMap<String, String>,
) -> Option<&'a str> {
    let canonical = crate::research::fetcher::canonical_host_pub(host);
    tracing::trace!(
        host,
        canonical = %canonical,
        "research::allowlist::bearer_token_env_for: resolving"
    );
    if let Some(name) = overrides.get(&canonical) {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            tracing::trace!(
                host,
                source = "override",
                "research::allowlist::bearer_token_env_for: hit override"
            );
            return Some(trimmed);
        }
    }
    let out = find_policy(host).and_then(|p| p.bearer_token_env);
    tracing::trace!(
        host,
        source = if out.is_some() {
            "static_policy"
        } else {
            "none"
        },
        "research::allowlist::bearer_token_env_for: resolved"
    );
    out
}

/// Hosts the bounded research fetcher is permitted to fetch from.
/// Tuple-order matches the order they were specced in proposal-04 §4.
///
/// Per-host env vars:
/// - `docs.rs` / `crates.io` / `github.com`: no auth (public
///   CDNs; an unknown Authorization header trips their
///   rate-limiters).
/// - `api.github.com`: `MOAGAN_RESEARCH_GITHUB_TOKEN`
///   (preserved as the v0.7.x `MOAGAN_RESEARCH_API_KEY`
///   fallback through the legacy `Config::research.api_key`
///   channel — see [`crate::research::fetcher::ResearchFetcher`]
///   for the wire-up).
///
/// Operators can override the env var name via `[research.auth]`
/// in `~/.config/moagan/config.toml`.
pub const HOSTS: &[HostPolicy] = &[
    HostPolicy {
        host: "docs.rs",
        auth_bearer: false,
        bearer_token_env: None,
    },
    HostPolicy {
        host: "crates.io",
        auth_bearer: false,
        bearer_token_env: None,
    },
    HostPolicy {
        host: "api.github.com",
        auth_bearer: true,
        bearer_token_env: Some("MOAGAN_RESEARCH_GITHUB_TOKEN"),
    },
    HostPolicy {
        host: "github.com",
        auth_bearer: false,
        bearer_token_env: None,
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
    let result = ALLOWED_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h));
    tracing::trace!(host, allowed = result, "research::allowlist::is_allowed");
    result
}

/// Look up the host policy entry that matches `host` (case-insensitive
/// on the hostname). Returns `None` when the host is not in the
/// allowlist; the caller is expected to enforce the allowlist
/// separately via [`is_allowed`] before looking up policy details.
pub fn find_policy(host: &str) -> Option<&'static HostPolicy> {
    let result = HOSTS.iter().find(|p| p.host.eq_ignore_ascii_case(host));
    tracing::trace!(
        host,
        found = result.is_some(),
        "research::allowlist::find_policy"
    );
    result
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

    /// K.4 sub-3: only `api.github.com` declares a per-host
    /// `bearer_token_env`. The other three hosts stay
    /// unauthenticated so a stale override or a typo in the
    /// `[research.auth]` map cannot smuggle an Authorization
    /// header onto `docs.rs` / `crates.io` / `github.com`.
    #[test]
    fn bearer_token_env_only_set_for_api_github_com() {
        assert_eq!(
            find_policy("api.github.com").unwrap().bearer_token_env,
            Some("MOAGAN_RESEARCH_GITHUB_TOKEN")
        );
        assert!(find_policy("docs.rs").unwrap().bearer_token_env.is_none());
        assert!(find_policy("crates.io").unwrap().bearer_token_env.is_none());
        assert!(
            find_policy("github.com")
                .unwrap()
                .bearer_token_env
                .is_none()
        );
    }

    /// K.4 sub-3: the operator override wins over the static
    /// default so the env var name is configurable without
    /// recompiling. The override map is keyed by the canonical
    /// hostname so `DOCS.RS` and `docs.rs` resolve the same.
    #[test]
    fn bearer_token_env_for_prefers_override() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("api.github.com".to_owned(), "CUSTOM_GH_TOKEN".to_owned());
        assert_eq!(
            bearer_token_env_for("api.github.com", &overrides),
            Some("CUSTOM_GH_TOKEN")
        );
        // Empty override value falls through to the static
        // default.
        overrides.insert("api.github.com".to_owned(), "   ".to_owned());
        assert_eq!(
            bearer_token_env_for("api.github.com", &overrides),
            Some("MOAGAN_RESEARCH_GITHUB_TOKEN")
        );
    }

    /// K.4 sub-3: hosts without a static `bearer_token_env`
    /// resolve to `None` regardless of the override map.
    /// Operators can opt a host into auth by adding an entry,
    /// but the absence of an entry never opts them in by
    /// accident.
    #[test]
    fn bearer_token_env_for_unknown_override_is_none() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("docs.rs".to_owned(), "DOCS_RS_TOKEN".to_owned());
        // The override IS used — this is the documented path
        // for opting `docs.rs` into auth.
        assert_eq!(
            bearer_token_env_for("docs.rs", &overrides),
            Some("DOCS_RS_TOKEN")
        );
        // Without an override, `docs.rs` stays at `None`.
        let empty = std::collections::HashMap::new();
        assert_eq!(bearer_token_env_for("docs.rs", &empty), None);
    }

    /// K.4 sub-3: the override map is consulted first, regardless
    /// of whether the host is in the allowlist. The allowlist
    /// filter lives in `fetch_one` via [`is_allowed`]; the override
    /// map only picks the env var name. This separation lets an
    /// operator pre-stage overrides for hosts that will be added to
    /// the allowlist in a follow-up PR without coordinating two
    /// config edits.
    #[test]
    fn bearer_token_env_for_respects_override_for_any_host() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("evil.example.com".to_owned(), "ANY_TOKEN".to_owned());
        assert_eq!(
            bearer_token_env_for("evil.example.com", &overrides),
            Some("ANY_TOKEN"),
            "override map wins regardless of allowlist membership"
        );
        // Without an override, hosts outside the allowlist fall
        // through to `None`.
        let empty = std::collections::HashMap::new();
        assert_eq!(
            bearer_token_env_for("evil.example.com", &empty),
            None,
            "no override + no allowlist entry => None"
        );
    }
}
