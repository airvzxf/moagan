//! Network policy for the sandboxed subprocess (catalog §D.11.13).
//!
//! [`NetworkPolicy`] is a closed enum that replaces the legacy
//! boolean `allow_network` flag on [`crate::sandbox::SandboxConfig`].
//! Three variants:
//!
//! - [`NetworkPolicy::Off`] — no network access (default; cargo runs
//!   offline because `CARGO_NET_OFFLINE=true` is injected in the
//!   subprocess env).
//! - [`NetworkPolicy::AllowList`] — only the listed hostnames are
//!   permitted (exact-string match).
//! - [`NetworkPolicy::Open`] — unrestricted network access (opt-in
//!   via config; `MOAGAN_SANDBOX_NETWORK_POLICY=open`).
//!
//! Enforcement is split across PRs:
//! - **D.11.13**: pre-execution host validation + warn-level logging
//!   via [`crate::sandbox::MoaSandbox::run_cmd`]. The subprocess is
//!   still spawned; the policy is informational at this stage.
//! - **D.11.7 (planned)**: seccomp-based syscall filtering.
//! - **D.11.1 (planned)**: cgroup-based resource controls.
//!
//! The cargo env hint `CARGO_NET_OFFLINE=true` is injected when the
//! policy is [`NetworkPolicy::Off`] so cargo refuses to fetch crates.
//! For `Open` and `AllowList`, the hint is NOT injected; the policy
//! is expected to be enforced at a lower layer (seccomp) once D.11.7
//! lands.
//!
//! Compliance: catalog `10-integrada-v0` §D.11.13.

use serde::{Deserialize, Serialize};

/// Closed enum for sandbox network policy.
///
/// Replaces the boolean `allow_network` flag with a typed value so the
/// default-deny posture is explicit and the "allowlist of hosts" case
/// (catalog §D.11.13) is representable without additional state.
///
/// `AllowList` is a struct variant rather than a tuple variant so the
/// internally-tagged serde representation (`{"kind":"allow_list",
/// "hosts":["a","b"]}`) is representable: serde rejects
/// `#[serde(tag = "...")]` on tuple variants whose inner value is a
/// sequence. The struct variant gives the inner `Vec<String>` a
/// named field (`hosts`) and the cost is one extra `hosts:` token
/// at construction sites.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// No network access. Default.
    #[default]
    Off,
    /// Only connections to the listed hostnames are permitted
    /// (exact-string match). The list is the canonical catalog
    /// representation: any host not in the list is denied.
    #[serde(rename = "allow_list")]
    AllowList {
        /// Hostnames the sandbox subprocess may reach.
        hosts: Vec<String>,
    },
    /// Unrestricted network access. Opt-in only.
    Open,
}

impl NetworkPolicy {
    /// Returns true iff `host` is permitted under this policy.
    ///
    /// - `Off`: always `false`.
    /// - `Open`: always `true`.
    /// - `AllowList { hosts }`: `true` iff `host` matches one of the
    ///   list entries exactly (no glob, no suffix matching).
    pub fn allows(&self, host: &str) -> bool {
        match self {
            Self::Off => false,
            Self::Open => true,
            Self::AllowList { hosts } => {
                let matched = hosts.iter().find(|h| h.as_str() == host);
                tracing::trace!(
                    sandbox = "policy",
                    host = %host,
                    allowlist_len = hosts.len(),
                    matched = matched.is_some(),
                    "NetworkPolicy::allows (AllowList)"
                );
                matched.is_some()
            }
        }
    }

    /// If the policy denies `host`, returns a human-readable reason
    /// suitable for logging or surfacing to the operator. Returns
    /// `None` when the policy permits the host.
    pub fn deny_reason(&self, host: &str) -> Option<String> {
        let reason = match self {
            Self::Off => Some(format!("network disabled (host={host})")),
            Self::AllowList { hosts } if !hosts.iter().any(|h| h == host) => {
                Some(format!("host {host} not in allow_list"))
            }
            _ => None,
        };
        if let Some(ref r) = reason {
            tracing::trace!(
                sandbox = "policy",
                host = %host,
                reason = %r,
                "NetworkPolicy::deny_reason produced denial"
            );
        }
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_list(hosts: &[&str]) -> NetworkPolicy {
        NetworkPolicy::AllowList {
            hosts: hosts.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn network_policy_default_is_off() {
        let policy = NetworkPolicy::default();
        assert_eq!(policy, NetworkPolicy::Off);
        assert!(!policy.allows("any.example.com"));
    }

    #[test]
    fn network_policy_off_denies_all_hosts() {
        let policy = NetworkPolicy::Off;
        assert!(!policy.allows("localhost"));
        assert!(!policy.allows("example.com"));
        assert!(!policy.allows(""));
        let reason = policy.deny_reason("example.com").unwrap();
        assert!(reason.contains("network disabled"));
        assert!(reason.contains("example.com"));
    }

    #[test]
    fn network_policy_open_allows_all_hosts() {
        let policy = NetworkPolicy::Open;
        assert!(policy.allows("localhost"));
        assert!(policy.allows("example.com"));
        assert!(policy.allows("anything"));
        assert_eq!(policy.deny_reason("example.com"), None);
    }

    #[test]
    fn network_policy_allow_list_allows_listed_hosts() {
        let policy = allow_list(&["crates.io", "github.com"]);
        assert!(policy.allows("crates.io"));
        assert!(policy.allows("github.com"));
        assert_eq!(policy.deny_reason("crates.io"), None);
        assert_eq!(policy.deny_reason("github.com"), None);
    }

    #[test]
    fn network_policy_allow_list_denies_unlisted() {
        let policy = allow_list(&["crates.io"]);
        assert!(!policy.allows("github.com"));
        assert!(!policy.allows("example.com"));
        let reason = policy.deny_reason("example.com").unwrap();
        assert!(reason.contains("example.com"));
        assert!(reason.contains("allow_list"));
    }

    #[test]
    fn network_policy_allow_list_empty_denies_all() {
        // An empty allowlist is semantically equivalent to Off
        // (no host is in the list, so every host is denied).
        let policy = allow_list(&[]);
        assert!(!policy.allows("any.host"));
        assert!(policy.deny_reason("any.host").is_some());
    }

    #[test]
    fn network_policy_serializes_to_snake_case() {
        let json = serde_json::to_string(&NetworkPolicy::Off).unwrap();
        assert!(
            json.contains("off"),
            "Off variant must serialise as snake_case, got {json}"
        );
        let json = serde_json::to_string(&NetworkPolicy::Open).unwrap();
        assert!(
            json.contains("open"),
            "Open variant must serialise as snake_case, got {json}"
        );
        let json = serde_json::to_string(&allow_list(&["a"])).unwrap();
        assert!(
            json.contains("allow_list"),
            "AllowList variant must serialise as allow_list, got {json}"
        );
    }

    #[test]
    fn network_policy_deserializes_from_json() {
        let policy: NetworkPolicy = serde_json::from_str(r#"{"kind":"off"}"#).unwrap();
        assert_eq!(policy, NetworkPolicy::Off);
        let policy: NetworkPolicy = serde_json::from_str(r#"{"kind":"open"}"#).unwrap();
        assert_eq!(policy, NetworkPolicy::Open);
        let policy: NetworkPolicy =
            serde_json::from_str(r#"{"kind":"allow_list","hosts":["a","b"]}"#).unwrap();
        assert_eq!(policy, allow_list(&["a", "b"]));
    }

    #[test]
    fn network_policy_round_trip_preserves_value() {
        let original = allow_list(&["crates.io", "github.com"]);
        let json = serde_json::to_string(&original).unwrap();
        let back: NetworkPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }
}
