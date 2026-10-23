//! Cross-process saves for the `<MOAGAN_HOME>/*_auto.toml` probe
//! sidecars (`max_tokens`, temperature, `top_p`, `top_k`).
//!
//! Each table used to write its whole in-memory snapshot over the
//! file. The in-process `disk_io` mutex only ordered writers inside
//! one process, so two moagan runs probing at the same time dropped
//! each other's entries (#1060). A save now holds an exclusive lock
//! on `<file>.lock`, re-reads the file and folds this process's
//! entries into it: an entry on disk that is newer (another process
//! verified it later) or that this process never held stays.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::Path;

use crate::error::{Error, IoError};

type Result<T> = std::result::Result<T, Error>;

/// A sidecar entry that records when it was last probed or verified.
pub(crate) trait Stamped {
    /// RFC 3339 timestamp of the last successful probe or verification.
    fn verified_at(&self) -> &str;
}

impl Stamped for super::probe::Entry {
    fn verified_at(&self) -> &str {
        &self.verified_at
    }
}

impl Stamped for super::temperature_probe::Entry {
    fn verified_at(&self) -> &str {
        &self.verified_at
    }
}

impl Stamped for super::top_p_probe::Entry {
    fn verified_at(&self) -> &str {
        &self.verified_at
    }
}

impl Stamped for super::top_k_probe::Entry {
    fn verified_at(&self) -> &str {
        &self.verified_at
    }
}

/// Run `f` while holding an exclusive lock on `<path>.lock`, which
/// every moagan process takes before it reads and rewrites `path`.
/// The lock is released when the lock file handle drops.
pub(crate) fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = std::path::PathBuf::from(lock_name);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io(IoError::Raw(e)))?;
    }
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    lock_file.lock().map_err(|e| Error::Io(IoError::Raw(e)))?;
    tracing::trace!(lock = %lock_path.display(), "sidecar_merge: lock held");
    f()
}

/// Fold this process's `mine` entries into the `disk` map. Each one
/// replaces the disk entry for its `(provider, model)` unless the disk
/// entry was verified later. Entries only on disk stay as they are.
pub(crate) fn merge_entries<E: Stamped + Clone>(
    disk: &mut BTreeMap<String, BTreeMap<String, E>>,
    mine: impl IntoIterator<Item = ((String, String), E)>,
) {
    for ((provider, model), entry) in mine {
        let models = disk.entry(provider).or_default();
        let keep_disk = models
            .get(&model)
            .is_some_and(|on_disk| is_newer(on_disk.verified_at(), entry.verified_at()));
        if !keep_disk {
            models.insert(model, entry);
        }
    }
}

/// Whether timestamp `a` is strictly later than `b`. Both are parsed
/// as RFC 3339; if either does not parse, the comparison falls back to
/// the raw strings so a malformed stamp never panics the save.
fn is_newer(a: &str, b: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(a), Ok(b)) => a > b,
        _ => a > b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct E(&'static str, u32);

    impl Stamped for E {
        fn verified_at(&self) -> &str {
            self.0
        }
    }

    fn key(p: &str, m: &str) -> (String, String) {
        (p.to_owned(), m.to_owned())
    }

    /// Entries another process saved, which this process never held,
    /// survive the save.
    #[test]
    fn entries_only_on_disk_stay() {
        let mut disk: BTreeMap<String, BTreeMap<String, E>> = BTreeMap::new();
        disk.entry("or".into())
            .or_default()
            .insert("a".into(), E("2026-10-23T10:00:00+00:00", 1));
        merge_entries(
            &mut disk,
            [(key("or", "b"), E("2026-10-23T11:00:00+00:00", 2))],
        );
        assert_eq!(disk["or"]["a"], E("2026-10-23T10:00:00+00:00", 1));
        assert_eq!(disk["or"]["b"], E("2026-10-23T11:00:00+00:00", 2));
    }

    /// The newer of the two copies of an entry wins, whichever side
    /// it is on; fractional seconds of different length compare by
    /// time, not by text.
    #[test]
    fn the_newer_copy_of_an_entry_wins() {
        let mut disk: BTreeMap<String, BTreeMap<String, E>> = BTreeMap::new();
        disk.entry("or".into())
            .or_default()
            .insert("a".into(), E("2026-10-23T10:00:00.5+00:00", 1));
        merge_entries(
            &mut disk,
            [(key("or", "a"), E("2026-10-23T10:00:00.25+00:00", 2))],
        );
        assert_eq!(disk["or"]["a"].1, 1, "disk copy is newer and stays");
        merge_entries(
            &mut disk,
            [(key("or", "a"), E("2026-10-23T10:00:01+00:00", 3))],
        );
        assert_eq!(
            disk["or"]["a"].1, 3,
            "this process's newer copy replaces it"
        );
    }

    /// The lock file sits next to the sidecar and `f`'s result comes
    /// back unchanged.
    #[test]
    fn with_file_lock_runs_f_under_a_sibling_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("max_tokens_auto.toml");
        let got = with_file_lock(&path, || Ok(7)).unwrap();
        assert_eq!(got, 7);
        assert!(tmp.path().join("max_tokens_auto.toml.lock").is_file());
    }
}
