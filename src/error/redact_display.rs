//! D.16.2: `Display` wrapper that applies `RedactPolicy`
//! automatically. Wraps any `Display` implementation and
//! pipes the rendered output through the policy so secrets
//! embedded in error messages (provider keys, file paths
//! inside homedirs, etc.) never reach the log line.
//!
//! The default surface is `Surface::Storage` because error
//! Display output is typically emitted to logs and
//! telemetry streams rather than the user's prompt. The
//! caller can override the surface by dropping down to
//! `crate::redact::apply` directly.

use crate::redact::{RedactPolicy, Surface, apply};

/// `Display` adapter that runs the inner implementor's
/// output through `RedactPolicy::default()` before writing
/// it to the formatter.
pub struct RedactedDisplay<'a, T: ?Sized> {
    inner: &'a T,
}

impl<'a, T: ?Sized> RedactedDisplay<'a, T> {
    /// Wrap a reference to any value that implements
    /// `Display` so its rendered output is redacted.
    pub fn new(inner: &'a T) -> Self {
        Self { inner }
    }
}

impl<'a, T: std::fmt::Display + ?Sized> std::fmt::Display for RedactedDisplay<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut raw = String::new();
        use std::fmt::Write as _;
        write!(raw, "{}", self.inner).expect("writing to String never fails");
        let policy = RedactPolicy::default();
        let redacted = apply(&policy, Surface::Storage, &raw)
            .map(|cow| cow.into_owned())
            .unwrap_or(raw);
        write!(f, "{redacted}")
    }
}

/// Convenience constructor that wraps `inner` in a
/// `RedactedDisplay`. Use this at call sites that need an
/// `impl Display` rather than a `Display` adapter struct.
pub fn redacted_display<T: ?Sized + std::fmt::Display>(inner: &T) -> RedactedDisplay<'_, T> {
    RedactedDisplay::new(inner)
}
