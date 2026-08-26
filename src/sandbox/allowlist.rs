//! Command policy for the subprocess sandbox.
//!
//! [`Allowlist`] is a positive list: every command the sandbox may
//! spawn must be present. [`Denylist`] is a hard blacklist applied to
//! the command AND each argument: even an allowlisted command that
//! gets a denied token as an argument is rejected.
//!
//! The split mirrors the proposal's defence-in-depth model:
//! - **Allowlist** says "these binaries are fine in principle".
//! - **Denylist** says "these specific tokens are never fine,
//!   regardless of which binary carries them" (e.g. `curl`, `wget`,
//!   `nc` for network exfiltration; `rm -rf /` for filesystem wipes).
//!
//! Compliance: `proposal-02-rust.md` §7.2 + catalog 10-integrada-v0 §D.11.3.

use std::collections::HashSet;

/// Commands the sandbox is allowed to spawn by default.
///
/// These match the list documented in `proposal-02-rust.md` §7.2
/// (the validation toolkit we expect on a developer workstation).
pub const DEFAULT_ALLOWLIST: &[&str] = &[
    // Rust
    "cargo", "rustc", "rustup", // Python
    "python", "python3", "pip", // TypeScript / Node
    "tsc", "node", "npm", "npx", // SQL
    "psql", "sqlite3", // Inspection helpers
    "jq", "cat", "ls", "find", "grep", "head", "tail", "echo", "wc", // Tests
    "sh",
];

/// Tokens that must never appear as either a command or an argument.
///
/// These cover the most obvious exfiltration / destruction vectors.
/// The list is intentionally short: the allowlist already prevents
/// random binaries; the denylist only catches the few exceptions we
/// care about even when the surrounding binary looks legitimate
/// (e.g. an `eval` token slipped into a `sh -c` invocation).
pub const DEFAULT_DENYLIST: &[&str] = &[
    "curl", "wget", "ssh", "scp", "rsync", "nc", "ncat", "socat", "eval", "exec", "source",
];

/// Positive list of allowed command basenames.
#[derive(Debug, Clone)]
pub struct Allowlist {
    inner: HashSet<String>,
}

impl Allowlist {
    /// Build an allowlist from a slice of basenames. Order does not
    /// matter; duplicates collapse.
    pub fn from_slice<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let inner: HashSet<String> = items
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect::<HashSet<_>>();
        tracing::debug!(
            sandbox = "allowlist",
            entries = inner.len(),
            "Allowlist::from_slice built allowlist"
        );
        Self { inner }
    }

    /// Use the project default allowlist.
    pub fn default_list() -> Self {
        tracing::info!(
            sandbox = "allowlist",
            entries = DEFAULT_ALLOWLIST.len(),
            "Allowlist::default_list loading project defaults"
        );
        Self::from_slice(DEFAULT_ALLOWLIST)
    }

    /// Returns true when `cmd` (the binary basename) is allowed.
    pub fn permits(&self, cmd: &str) -> bool {
        let key = basename(cmd);
        let allowed = self.inner.contains(key);
        tracing::trace!(
            sandbox = "allowlist",
            cmd = %cmd,
            key = %key,
            allowed,
            "Allowlist::permits lookup"
        );
        allowed
    }

    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true when the allowlist has no entries. The sandbox
    /// rejects every command in that state.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for Allowlist {
    fn default() -> Self {
        Self::default_list()
    }
}

/// Hard denylist. Used both for the command itself and to scan the
/// full argv.
#[derive(Debug, Clone)]
pub struct Denylist {
    inner: HashSet<String>,
}

impl Denylist {
    /// Build a denylist from a slice of tokens.
    pub fn from_slice<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let inner: HashSet<String> = items
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect::<HashSet<_>>();
        tracing::debug!(
            sandbox = "denylist",
            entries = inner.len(),
            "Denylist::from_slice built denylist"
        );
        Self { inner }
    }

    /// Use the project default denylist.
    pub fn default_list() -> Self {
        tracing::info!(
            sandbox = "denylist",
            entries = DEFAULT_DENYLIST.len(),
            "Denylist::default_list loading project defaults"
        );
        Self::from_slice(DEFAULT_DENYLIST)
    }

    /// Returns true when the token is on the denylist. Comparison is
    /// exact (no globbing) so it stays cheap; the allowlist already
    /// does the bulk filtering.
    pub fn bans(&self, token: &str) -> bool {
        let banned = self.inner.contains(token);
        tracing::trace!(
            sandbox = "denylist",
            token = %token,
            banned,
            "Denylist::bans lookup"
        );
        banned
    }

    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true when the denylist has no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for Denylist {
    fn default() -> Self {
        Self::default_list()
    }
}

/// True if `cmd` is permitted by `allowlist`.
pub fn is_allowed(cmd: &str, allowlist: &Allowlist) -> bool {
    let allowed = allowlist.permits(cmd);
    tracing::trace!(
        sandbox = "allowlist",
        cmd = %cmd,
        allowed,
        "is_allowed check"
    );
    allowed
}

/// True if the argv contains any token from the denylist. The check
/// runs on every element, including argv[0] (the command basename).
pub fn contains_deny_token<S>(argv: &[S], denylist: &Denylist) -> bool
where
    S: AsRef<str>,
{
    let mut banned_token: Option<String> = None;
    for a in argv {
        if denylist.bans(a.as_ref()) {
            banned_token = Some(a.as_ref().to_owned());
            break;
        }
    }
    match banned_token {
        Some(tok) => {
            tracing::warn!(
                sandbox = "denylist",
                token = %tok,
                "argv contains a denylisted token"
            );
            true
        }
        None => false,
    }
}

/// Extract the basename from a possibly-path-prefixed binary name.
/// `cargo` stays `cargo`; `/usr/local/bin/cargo` becomes `cargo`.
fn basename(cmd: &str) -> &str {
    cmd.rsplit(['/', '\\']).next().unwrap_or(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allowlist_permits_cargo() {
        let a = Allowlist::default();
        assert!(a.permits("cargo"));
        assert!(a.permits("/usr/local/bin/cargo"));
        assert!(!a.permits("malware"));
    }

    #[test]
    fn default_allowlist_includes_helpers() {
        let a = Allowlist::default();
        assert!(a.permits("python3"));
        assert!(a.permits("tsc"));
        assert!(a.permits("node"));
        assert!(a.permits("sqlite3"));
        assert!(a.permits("cat"));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let a = Allowlist::from_slice::<std::iter::Empty<&str>, _>(std::iter::empty());
        assert!(a.is_empty());
        assert!(!a.permits("cargo"));
    }

    #[test]
    fn default_denylist_blocks_network_tools() {
        let d = Denylist::default();
        assert!(d.bans("curl"));
        assert!(d.bans("wget"));
        assert!(d.bans("nc"));
        assert!(!d.bans("cargo"));
    }

    #[test]
    fn contains_deny_token_scans_argv() {
        let d = Denylist::default();
        assert!(contains_deny_token(&["cargo", "test", "curl"], &d));
        assert!(!contains_deny_token(&["cargo", "test"], &d));
        let empty: &[&str] = &[];
        assert!(!contains_deny_token(empty, &d));
    }

    #[test]
    fn basename_handles_paths() {
        assert_eq!(basename("cargo"), "cargo");
        assert_eq!(basename("/usr/local/bin/cargo"), "cargo");
        assert_eq!(basename("/usr/bin/python3"), "python3");
        assert_eq!(basename("C:\\bin\\cargo.exe"), "cargo.exe");
    }
}
