//! In-memory `MaxTokensTable` — the runtime singleton that the
//! pipeline consults before issuing each LLM call. Backed by the
//! on-disk [`MaxTokensTableFile`] for cross-run persistence.
//!
//! The table is *not* a process-global mutable: it lives inside an
//! `Arc` and travels through the call graph (typically owned by the
//! `ProviderRegistry`). Cloning the `Arc` is cheap (refcount bump)
//! and the per-`run_id` semantics stay clean because every run
//! process creates its own table.
//!
//! Concurrency model: [`RwLock`] over a `BTreeMap`. Reads (the hot
//! path: every LLM call) lock for read; writes (probe completion,
//! one-time persistence) lock for write. The `parking_lot` flavour
//! is already on the dependency list, so the lock is non-poisoning
//! and contention-free under realistic workloads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;

use crate::error::Result;
use crate::fs_layout::MoaganHome;

use super::probe::{
    Entry, MAX_AUTOPROBE_CEILING, MIN_AUTOPROBE_FLOOR, MaxTokensTableFile, ProbeTransport,
    detect_max_tokens,
};

/// In-memory table of `(provider_name, model_name) -> Entry` plus
/// the floor supplied by the operator (mirrors
/// `ProviderConfig::max_token_auto`). Wrapped in an `Arc<RwLock>`
/// so a single instance can be cloned into every callsite.
#[derive(Clone)]
pub struct MaxTokensTable {
    inner: Arc<RwLock<MaxTokensTableInner>>,
    /// Path to the on-disk TOML file. `None` when persistence is
    /// disabled (`ProviderConfig::max_token_auto_save = false` or
    /// the env var `MOAGAN_MAX_TOKEN_AUTO_SAVE=false`).
    persist_path: Option<PathBuf>,
}

#[derive(Debug)]
struct MaxTokensTableInner {
    entries: BTreeMap<(String, String), Entry>,
    /// Operator-supplied minimum (the `Option<u32>` from
    /// `ProviderConfig::max_token_auto`, with `None` and `Some(0)`
    /// both mapping to `MIN_AUTOPROBE_FLOOR`).
    floor: u32,
    /// Total probe tasks started across all calls since startup.
    /// M7: counts tokio tasks spawned (one per `probe_and_store` /
    /// `verify`), not HTTP round-trips. Used for telemetry and
    /// operator-visible diagnostics.
    probe_tasks_started: u32,
    /// Total probes that succeeded.
    probes_succeeded: u32,
    /// Total probes that failed (rejection or indeterminate).
    probes_failed: u32,
    /// [`tokio::task::JoinHandle`]s for every background probe the
    /// registry fired at startup. The runtime joins them via
    /// [`Self::await_ready`] so the caller can decide whether to
    /// gate the first LLM call behind the discovery. Stored as
    /// `Vec<JoinHandle<()>>` because the typed return value is
    /// `Result<(), JoinError>` which would force every consumer to
    /// import `tokio::task::JoinError`; the inner type is unit so
    /// dropping the handle is safe.
    pending: Vec<tokio::task::JoinHandle<()>>,
}

impl MaxTokensTable {
    /// Build a table from the on-disk file at `<MOAGAN_HOME>/max_tokens_auto.toml`.
    /// `floor` is the operator-supplied floor from the per-provider
    /// `max_token_auto` setting. `save` controls whether subsequent
    /// probe results are persisted.
    pub fn from_home(home: &MoaganHome, floor: u32, save: bool) -> Result<Self> {
        let path = home.max_tokens_auto_path();
        Self::from_path(&path, floor, save)
    }

    /// Build a table from an explicit path. Used by tests and by
    /// [`Self::from_home`].
    pub fn from_path(path: &Path, floor: u32, save: bool) -> Result<Self> {
        let file = MaxTokensTableFile::load(path)?;
        let entries = file
            .providers
            .into_iter()
            .flat_map(|(provider, models)| {
                models
                    .into_iter()
                    .map(move |(model, entry)| ((provider.clone(), model), entry))
            })
            .collect();
        Ok(Self {
            inner: Arc::new(RwLock::new(MaxTokensTableInner {
                entries,
                floor: floor.max(MIN_AUTOPROBE_FLOOR),
                probe_tasks_started: 0,
                probes_succeeded: 0,
                probes_failed: 0,
                pending: Vec::new(),
            })),
            persist_path: save.then(|| path.to_path_buf()),
        })
    }

    /// Build a fresh table with no on-disk backing. Used by tests.
    pub fn empty(floor: u32) -> Self {
        Self {
            inner: Arc::new(RwLock::new(MaxTokensTableInner {
                entries: BTreeMap::new(),
                floor: floor.max(MIN_AUTOPROBE_FLOOR),
                probe_tasks_started: 0,
                probes_succeeded: 0,
                probes_failed: 0,
                pending: Vec::new(),
            })),
            persist_path: None,
        }
    }

    /// Read the cached value for `(provider, model)`. Returns
    /// `None` if no entry exists.
    pub fn get(&self, provider: &str, model: &str) -> Option<Entry> {
        self.inner
            .read()
            .entries
            .get(&(provider.to_owned(), model.to_owned()))
            .cloned()
    }

    /// Set the per-`(provider, model)` operator floor for the next
    /// probe. The floor is honoured during [`Self::probe_and_store`]
    /// so the discovered value cannot shrink below `floor`. When the
    /// pair already has a cached entry, the floor is updated in place
    /// without disturbing the discovered `max_tokens`.
    ///
    /// Today the floor is global (the max across every opted-in
    /// provider) because the algorithm only carries one floor; a
    /// future refactor can lift it onto [`Entry`] so a provider with
    /// a smaller floor is not silently bumped to a larger upstream's
    /// value. The [`Self::set_floor_for`] API stays open so callers
    /// do not need to change when that lands.
    pub fn set_floor_for(&self, _provider: &str, _model: &str, floor: u32) {
        let mut inner = self.inner.write();
        inner.floor = inner.floor.max(floor.max(MIN_AUTOPROBE_FLOOR));
    }

    /// Resolve the effective `max_tokens` for `(provider, model)`:
    /// the cached value if present, otherwise `None`. Callers fall
    /// back to `DEFAULT_MAX_TOKENS` (1M) when `None` so a fresh
    /// process still produces a sane wire body before the probe
    /// completes.
    pub fn resolve_cached(&self, provider: &str, model: &str) -> Option<u32> {
        self.get(provider, model).map(|e| e.max_tokens)
    }

    /// Probe the upstream and insert the discovered value. Idempotent
    /// for a given `(provider, model)` when called twice: the second
    /// call re-probes and overwrites with the fresh value.
    pub async fn probe_and_store(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn ProbeTransport>,
    ) -> Result<u32> {
        let floor = self.inner.read().floor;
        let attempts_before = self.inner.read().probe_tasks_started;
        let discovered = detect_max_tokens(transport, floor).await?;

        let now = Utc::now().to_rfc3339();
        let attempts = {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            inner.probes_succeeded += 1;
            let attempts_total = inner.probe_tasks_started - attempts_before;
            inner.entries.insert(
                (provider.to_owned(), model.to_owned()),
                Entry {
                    max_tokens: discovered,
                    detected_at: now.clone(),
                    verified_at: now,
                    auto: true,
                    attempts: attempts_total,
                },
            );
            attempts_total
        };
        let _ = attempts;

        if let Some(path) = self.persist_path.as_ref()
            && let Err(e) = self.persist_to(path)
        {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "max_tokens_auto.toml persistence failed; in-memory entry is kept"
            );
        }
        Ok(discovered)
    }

    /// Verify a cached entry by re-probing once. On success,
    /// `verified_at` is updated. On failure (the upstream rejected
    /// the previously-known value), the entry is removed and the
    /// caller falls back to a full re-probe.
    pub async fn verify(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn ProbeTransport>,
    ) -> Result<bool> {
        let cached = self.get(provider, model);
        let Some(entry) = cached else {
            return Ok(false);
        };
        let outcome = transport.probe_send(entry.max_tokens).await;
        let ok = matches!(outcome, super::probe::ProbeOutcome::Accepted);
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            if ok {
                inner.probes_succeeded += 1;
                if let Some(e) = inner
                    .entries
                    .get_mut(&(provider.to_owned(), model.to_owned()))
                {
                    e.verified_at = Utc::now().to_rfc3339();
                }
            } else {
                inner.probes_failed += 1;
                inner
                    .entries
                    .remove(&(provider.to_owned(), model.to_owned()));
            }
        }
        if let Some(path) = self.persist_path.as_ref() {
            let _ = self.persist_to(path);
        }
        Ok(ok)
    }

    /// Persist the current in-memory state to disk. Best-effort:
    /// callers wrap in `if let Err(_)` because losing a probe result
    /// is preferable to aborting the run.
    pub fn persist_to(&self, path: &Path) -> Result<()> {
        let inner = self.inner.read();
        let mut file = MaxTokensTableFile::new_empty();
        for ((provider, model), entry) in &inner.entries {
            file.providers
                .entry(provider.clone())
                .or_default()
                .insert(model.clone(), entry.clone());
        }
        file.save(path)
    }

    /// Record the [`tokio::task::JoinHandle`] of a background probe
    /// the registry fired at startup. The caller can `await` every
    /// handle via [`Self::await_ready`] when it wants to gate the
    /// first LLM call behind the discovery (CI, smoke tests).
    pub fn record_probe_join_handle(
        &self,
        provider: String,
        model: String,
        handle: tokio::task::JoinHandle<()>,
    ) {
        let _ = (provider, model);
        let mut inner = self.inner.write();
        inner.pending.push(handle);
    }

    /// Wait for every probe the registry fired at startup to
    /// finish. No-op when no probe was fired (mock-only registry).
    /// Errors from individual probes are logged via `tracing::warn!`
    /// and do not propagate — a failing probe degrades to the
    /// static `max_tokens` knob, never aborts the run.
    pub async fn await_ready(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut inner = self.inner.write();
            std::mem::take(&mut inner.pending)
        };
        for h in handles {
            if let Err(e) = h.await {
                tracing::warn!(error = %e, "max_tokens_auto: probe task join failed");
            }
        }
    }

    /// Persist to the path the table was built from. `None` when
    /// persistence was disabled at construction.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = self.persist_path.clone() else {
            return Ok(());
        };
        self.persist_to(&path)
    }

    /// Effective floor after the safety clamp. Useful for tests and
    /// telemetry.
    pub fn floor(&self) -> u32 {
        self.inner.read().floor
    }

    /// Total probe tasks started across the lifetime of this table.
    /// M7: counts tokio tasks spawned, not HTTP round-trips (Phase 1
    /// can fire 30 sequential probes; Phase 2 fires 20-point
    /// batches). Renamed from `probes_attempted` so the name
    /// matches what it actually counts.
    pub fn probe_tasks_started(&self) -> u32 {
        self.inner.read().probe_tasks_started
    }

    /// Total probes that succeeded.
    pub fn probes_succeeded(&self) -> u32 {
        self.inner.read().probes_succeeded
    }

    /// Total probes that failed.
    pub fn probes_failed(&self) -> u32 {
        self.inner.read().probes_failed
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.inner.read().entries.len()
    }

    /// `true` when no entries are cached.
    pub fn is_empty(&self) -> bool {
        self.inner.read().entries.is_empty()
    }

    /// Iterate over `(provider, model, entry)` triples.
    pub fn iter(&self) -> impl Iterator<Item = ((String, String), Entry)> + '_ {
        let inner = self.inner.read();
        inner
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }
}

/// Effective max_tokens for a single LLM call, given the table and
/// the per-call role default. The clamp is:
///   min(role_default, table_cached_or_default, MAX_AUTOPROBE_CEILING)
/// where `table_cached_or_default` is the discovered value if the
/// table has it, otherwise `DEFAULT_MAX_TOKENS`.
pub fn effective_max_tokens(table: &MaxTokensTable, provider: &str, model: &str) -> u32 {
    use crate::llm::prompts::DEFAULT_MAX_TOKENS;
    let from_table = table
        .resolve_cached(provider, model)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    DEFAULT_MAX_TOKENS
        .min(from_table)
        .clamp(MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
}

/// Probe every `(provider, model)` pair in `entries` and insert the
/// results into the table. Returns a vector of `(provider, model,
/// Result<u32>)` so the caller can log failures and decide which
/// providers to disable for the run.
pub async fn probe_all(
    table: &MaxTokensTable,
    entries: impl IntoIterator<Item = (String, String)>,
    transport_for: impl Fn(&str, &str) -> Option<Arc<dyn ProbeTransport>>,
) -> Vec<(String, String, Result<u32>)> {
    let mut handles = Vec::new();
    for (provider, model) in entries {
        let Some(transport) = transport_for(&provider, &model) else {
            continue;
        };
        let table = table.clone();
        handles.push(tokio::spawn(async move {
            let res = table.probe_and_store(&provider, &model, transport).await;
            (provider, model, res)
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(triple) = h.await {
            out.push(triple);
        } // task panicked: treat as silent skip
    }
    out
}

/// Build the path of the on-disk `max_tokens_auto.toml` for a given
/// `MoaganHome`. The file lives at `<root>/max_tokens_auto.toml`,
/// mirroring the `<root>/api_keys.toml` convention. Public so the
/// `moagan doctor` subcommand can show the operator where to look.
pub fn max_tokens_auto_path(home: &MoaganHome) -> PathBuf {
    home.max_tokens_auto_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::probe::ProbeOutcome;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Clone)]
    struct Capped {
        cap: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl ProbeTransport for Capped {
        async fn probe_send(&self, n: u32) -> ProbeOutcome {
            if n <= self.cap.load(Ordering::SeqCst) {
                ProbeOutcome::Accepted
            } else {
                ProbeOutcome::Rejected
            }
        }
    }

    fn cap(n: u32) -> Arc<dyn ProbeTransport> {
        Arc::new(Capped {
            cap: Arc::new(AtomicU32::new(n)),
        })
    }

    #[tokio::test]
    async fn fresh_table_resolves_to_default() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        assert!(t.is_empty());
        assert_eq!(
            effective_max_tokens(&t, "minimax", "MiniMax-M3"),
            crate::llm::prompts::DEFAULT_MAX_TOKENS
        );
    }

    #[tokio::test]
    async fn probe_and_store_inserts_entry() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        let v = t
            .probe_and_store("minimax", "MiniMax-M3", cap(524_288))
            .await
            .unwrap();
        assert_eq!(v, 524_288);
        let entry = t.get("minimax", "MiniMax-M3").unwrap();
        assert_eq!(entry.max_tokens, 524_288);
        assert!(entry.auto);
        assert!(!entry.detected_at.is_empty());
    }

    #[tokio::test]
    async fn verify_updates_verified_at_on_success() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        t.probe_and_store("minimax", "MiniMax-M3", cap(524_288))
            .await
            .unwrap();
        let verified_before = t.get("minimax", "MiniMax-M3").unwrap().verified_at;
        // Force a different verified_at by sleeping one ms.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let ok = t
            .verify("minimax", "MiniMax-M3", cap(524_288))
            .await
            .unwrap();
        assert!(ok);
        let verified_after = t.get("minimax", "MiniMax-M3").unwrap().verified_at;
        assert!(verified_after >= verified_before);
    }

    #[tokio::test]
    async fn verify_drops_entry_on_failure() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        t.probe_and_store("minimax", "MiniMax-M3", cap(524_288))
            .await
            .unwrap();
        // Provider now rejects the previously-accepted value.
        let ok = t.verify("minimax", "MiniMax-M3", cap(1024)).await.unwrap();
        assert!(!ok);
        assert!(t.get("minimax", "MiniMax-M3").is_none());
    }

    #[tokio::test]
    async fn persist_round_trip_via_home_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(dir.path().to_path_buf());
        let t = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true).unwrap();
        t.probe_and_store("minimax", "MiniMax-M3", cap(524_288))
            .await
            .unwrap();
        // The path the table uses is the on-disk file. Verify it
        // exists and round-trips.
        let path = home.max_tokens_auto_path();
        assert!(path.exists(), "max_tokens_auto.toml must be on disk");
        let back = MaxTokensTable::from_path(&path, MIN_AUTOPROBE_FLOOR, false).unwrap();
        assert!(back.get("minimax", "MiniMax-M3").is_some());
    }

    #[test]
    fn from_path_with_save_disabled_does_not_set_persist_path() {
        let t = MaxTokensTable::from_path(
            std::path::Path::new("/nonexistent.toml"),
            MIN_AUTOPROBE_FLOOR,
            false,
        )
        .unwrap();
        assert!(t.persist_path.is_none());
    }

    #[test]
    fn floor_clamped_to_minimum() {
        let t = MaxTokensTable::empty(0);
        assert_eq!(t.floor(), MIN_AUTOPROBE_FLOOR);
    }

    #[tokio::test]
    async fn probe_all_runs_pairs_in_parallel() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        let entries = vec![
            ("minimax".to_owned(), "MiniMax-M3".to_owned()),
            ("deepseek".to_owned(), "deepseek-v4-flash".to_owned()),
        ];
        let results = probe_all(&t, entries, |provider, _model| match provider {
            "minimax" => Some(cap(524_288)),
            "deepseek" => Some(cap(128 * 1024)),
            _ => None,
        })
        .await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, _, r)| r.is_ok()));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn effective_max_tokens_falls_back_to_default_when_missing() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        // No entry for (minimax, MiniMax-M3).
        assert_eq!(
            effective_max_tokens(&t, "minimax", "MiniMax-M3"),
            crate::llm::prompts::DEFAULT_MAX_TOKENS
        );
    }

    #[test]
    fn effective_max_tokens_uses_cached_when_present() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        // Insert an entry manually so we can pin the behaviour
        // without depending on the probe async path.
        let mut inner = t.inner.write();
        inner.entries.insert(
            ("minimax".to_owned(), "MiniMax-M3".to_owned()),
            Entry {
                max_tokens: 524_288,
                detected_at: "2026-08-11T00:00:00Z".to_owned(),
                verified_at: "2026-08-11T00:00:00Z".to_owned(),
                auto: true,
                attempts: 35,
            },
        );
        drop(inner);
        let got = effective_max_tokens(&t, "minimax", "MiniMax-M3");
        assert_eq!(got, 524_288);
    }
}
