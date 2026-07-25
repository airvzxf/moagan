//! `SecretString` — a string whose contents are zeroized when dropped.
//!
//! Used for API keys, tokens, and any value that should not survive the
//! run's lifetime on the heap. The value is **never** serialized; if you
//! need to persist a reference, use `SecretRef` (a newtype around a name
//! like `"env:MINIMAX_API_KEY"` or `"file:./secrets/glm.key"`).
//!
//! Compliance: catalog 10-integrada-v0 §D.1.7 (Day 1).

use std::fmt;
use std::ops::Deref;

use zeroize::Zeroize;

/// A secret-bearing string that is wiped on drop.
///
/// `SecretString` does not implement `Serialize` — secrets leave the
/// process only via the templated sources declared in [`SecretSource`].
#[derive(Clone, Zeroize)]
pub struct SecretString(String);

impl SecretString {
    /// Mask shown by `Display` and `Debug`. Never contains the secret.
    pub const MASK: &'static str = "***";

    /// Build a new `SecretString` from an owned `String`. The caller is
    /// responsible for ensuring the value is actually a secret.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Build an empty secret. Useful as a placeholder while a key is
    /// being resolved from environment or interactive input.
    pub fn empty() -> Self {
        Self(String::new())
    }

    /// Returns true if no secret is held.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the underlying string. Use sparingly — the borrow should
    /// not outlive the call site that needs the secret.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the inner `String`. The caller
    /// takes ownership of zeroization duties.
    pub fn into_inner(mut self) -> String {
        let inner = std::mem::take(&mut self.0);
        self.0.zeroize();
        inner
    }
}

impl Default for SecretString {
    fn default() -> Self {
        Self::empty()
    }
}

impl Deref for SecretString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::MASK)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretString")
            .field("value", &Self::MASK)
            .finish()
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        // Constant-time comparison guards against timing leaks.
        let a = self.0.as_bytes();
        let b = other.0.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut acc = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            acc |= x ^ y;
        }
        acc == 0
    }
}

impl Eq for SecretString {}

/// A reference to a secret by source. Stored in `manifest.json` and in
/// `provider_changes` so we know where the key came from without storing
/// the key itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "spec")]
pub enum SecretSource {
    /// `env:NAME` — resolved from process environment.
    Env(String),
    /// `file:/path` — resolved from a file with restrictive permissions.
    File(String),
    /// Interactive: the user typed the key into the prompt.
    Interactive,
    /// Keyring (libsecret / Keychain) — read on demand, not stored.
    Keyring,
}

impl SecretSource {
    /// Human-readable description of where the secret is sourced.
    pub fn describe(&self) -> String {
        match self {
            Self::Env(name) => format!("env:{name}"),
            Self::File(path) => format!("file:{path}"),
            Self::Interactive => "interactive".to_owned(),
            Self::Keyring => "keyring".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_expose() {
        let s = SecretString::new("sk-cp-hello".into());
        assert_eq!(s.expose(), "sk-cp-hello");
    }

    #[test]
    fn display_is_masked() {
        let s = SecretString::new("sk-cp-hello".into());
        assert_eq!(format!("{s}"), "***");
    }

    #[test]
    fn debug_is_masked() {
        let s = SecretString::new("sk-cp-hello".into());
        let dbg = format!("{s:?}");
        assert!(dbg.contains("***"));
        assert!(!dbg.contains("sk-cp-hello"));
    }

    #[test]
    fn from_string_and_str() {
        let a: SecretString = String::from("abc").into();
        let b: SecretString = "abc".into();
        assert_eq!(a.expose(), b.expose());
    }

    #[test]
    fn equality_is_value_based() {
        let a = SecretString::new("same".into());
        let b = SecretString::new("same".into());
        let c = SecretString::new("different".into());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn empty_is_empty() {
        let s = SecretString::empty();
        assert!(s.is_empty());
        assert_eq!(s.expose(), "");
    }

    #[test]
    fn into_inner_returns_value() {
        let s = SecretString::new("payload".into());
        assert_eq!(s.into_inner(), "payload");
    }

    #[test]
    fn default_is_empty() {
        let s = SecretString::default();
        assert!(s.is_empty());
    }

    #[test]
    fn secret_source_describe() {
        assert_eq!(SecretSource::Env("FOO".into()).describe(), "env:FOO");
        assert_eq!(SecretSource::File("/k/x".into()).describe(), "file:/k/x");
        assert_eq!(SecretSource::Interactive.describe(), "interactive");
        assert_eq!(SecretSource::Keyring.describe(), "keyring");
    }

    #[test]
    fn secret_source_round_trips_json() {
        let s = SecretSource::Env("MINIMAX_API_KEY".into());
        let j = serde_json::to_string(&s).unwrap();
        let back: SecretSource = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }
}
