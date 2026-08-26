//! D.22.5: `StaleArtifact` log. Emits a structured `tracing::warn!`
//! when an artefact on disk is older than the run's
//! `last_resumed_at + TTL`. Used by the resume path to surface
//! "this run is reusing an artefact whose TTL has elapsed" without
//! failing the run — operators see a warning in the telemetry
//! stream and can decide whether to invalidate downstream phases.
//!
//! Spec contract:
//!
//! - [`StaleArtifact::emit`] writes one `tracing::warn!` event
//!   tagged `event = "stale_artifact"` with `path`, `age_secs`,
//!   and `ttl_secs` fields. The `tracing-subscriber` JSON layer
//!   (T01-06 §5.4) routes the event to `telemetry/calls.jsonl.gz`
//!   so post-mortems can grep for it.
//! - [`detect_stale`] is a pure function over the filesystem:
//!   `Some(StaleArtifact)` when the artefact's mtime is older
//!   than `ttl_secs`, `None` otherwise. A missing or
//!   un-stat-able file resolves to `None` so the helper never
//!   panics on a transient race.
//!
//! The function is intentionally cheap: one `metadata()` call,
//! one `elapsed()` call. No directory walk, no glob.

use std::path::{Path, PathBuf};

/// Description of a stale artefact the resume path detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleArtifact {
    /// The on-disk path that is older than the TTL.
    pub path: PathBuf,
    /// Age of the artefact in seconds, measured against
    /// `SystemTime::now()` at the moment of the check.
    pub age_secs: u64,
    /// The TTL the caller configured for this run. Echoed in
    /// the log so the operator can see both numbers without
    /// re-reading the config.
    pub ttl_secs: u64,
}

impl StaleArtifact {
    /// Emit a structured `tracing::warn!` event. The event is
    /// tagged with `event = "stale_artifact"` so a JSON grep
    /// over `telemetry/calls.jsonl.gz` finds every stale
    /// artefact the resume path noticed.
    pub fn emit(&self) {
        tracing::trace!(
            path = %self.path.display(),
            age_secs = self.age_secs,
            ttl_secs = self.ttl_secs,
            "StaleArtifact::emit"
        );
        tracing::warn!(
            event = "stale_artifact",
            path = %self.path.display(),
            age_secs = self.age_secs,
            ttl_secs = self.ttl_secs,
            "StaleArtifact detected"
        );
    }
}

/// Inspect `path` and return `Some(StaleArtifact)` when the
/// file's mtime is older than `ttl_secs` seconds. `None` for
/// any other case (file missing, stat failed, age within TTL).
/// Never panics.
///
/// The comparison runs in milliseconds (not whole seconds) so
/// a TTL of 0 reliably flags every file with any non-zero
/// age. With `ttl_secs = u64::MAX` no file is ever considered
/// stale in practice; with `ttl_secs = 0` only a file whose
/// mtime is exactly the moment of the call (zero-millisecond
/// age) is "fresh", which is the safe direction for a
/// resume-time check.
pub fn detect_stale(path: &Path, ttl_secs: u64) -> Option<StaleArtifact> {
    tracing::trace!(
        path = %path.display(),
        ttl_secs,
        "detect_stale: enter"
    );
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::trace!(
                path = %path.display(),
                error = %e,
                "detect_stale: metadata() failed; returning None"
            );
            return None;
        }
    };
    let modified = match meta.modified() {
        Ok(m) => m,
        Err(e) => {
            tracing::trace!(
                path = %path.display(),
                error = %e,
                "detect_stale: modified() failed; returning None"
            );
            return None;
        }
    };
    let age = match modified.elapsed() {
        Ok(a) => a,
        Err(e) => {
            tracing::trace!(
                path = %path.display(),
                error = %e,
                "detect_stale: elapsed() failed; returning None"
            );
            return None;
        }
    };
    let age_secs = age.as_secs();
    let age_ms = age.as_millis();
    let ttl_ms = u128::from(ttl_secs).saturating_mul(1000);
    if age_ms > ttl_ms {
        tracing::debug!(
            path = %path.display(),
            age_secs,
            ttl_secs,
            "detect_stale: stale"
        );
        Some(StaleArtifact {
            path: path.to_path_buf(),
            age_secs,
            ttl_secs,
        })
    } else {
        tracing::trace!(
            path = %path.display(),
            age_secs,
            ttl_secs,
            "detect_stale: fresh"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh file (mtime ≈ now) under an effectively infinite
    /// TTL must resolve to `None`. Pins the common case: the
    /// resume path looks at the artefact, sees a young mtime,
    /// and moves on without emitting a warning.
    #[test]
    fn detect_stale_returns_none_when_fresh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("fresh.txt");
        std::fs::write(&path, b"hello").expect("write");
        let result = detect_stale(&path, u64::MAX);
        assert!(result.is_none(), "fresh artefact must not be stale");
    }

    /// A file with a real mtime and `ttl_secs = 0` is always
    /// stale (any non-zero age is > 0). Pins the "older than
    /// TTL" comparison and the `Some` return path. The test
    /// sleeps 10 ms after `write` so the mtime reliably lands
    /// in the past even on a coarse-grained filesystem.
    #[test]
    fn detect_stale_returns_some_when_old() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("old.txt");
        std::fs::write(&path, b"hello").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = detect_stale(&path, 0);
        let artifact = result.expect("age > 0 must surface as stale");
        assert_eq!(artifact.path, path);
        assert_eq!(artifact.ttl_secs, 0);
        assert!(artifact.age_secs < 60);
    }

    /// `emit` does not panic and does not require a tracing
    /// subscriber to be installed. Pins the call shape so a
    /// future refactor that drops a field surfaces as a
    /// compile error.
    #[test]
    fn stale_artifact_emit_does_not_panic() {
        let artifact = StaleArtifact {
            path: PathBuf::from("/tmp/moagan-stale-test"),
            age_secs: 42,
            ttl_secs: 10,
        };
        artifact.emit();
    }

    /// A missing file resolves to `None`, never an error. The
    /// resume path uses this to distinguish "artefact gone"
    /// (caller decides what to do) from "artefact stale"
    /// (caller emits a warning).
    #[test]
    fn detect_stale_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist.txt");
        assert!(detect_stale(&missing, 0).is_none());
    }
}
