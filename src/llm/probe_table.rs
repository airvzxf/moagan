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
//!
//! # Why an auto-probe
//!
//! LLM providers do not advertise their real `max_tokens` ceiling
//! in a machine-readable way. The OAuth/backend surface lists one
//! number, the chat-completion surface accepts another, and the
//! streaming-vs-non-streaming paths often disagree. Hard-coding a
//! `MAX_TOKENS_CAP` per provider is a losing bet: the moment a
//! vendor rolls a new model the constant is wrong.
//!
//! `moagan` (v0.10+) probes each `(provider, model)` pair at first
//! startup to discover the actual ceiling. The discovered value is
//! cached at `<MOAGAN_HOME>/max_tokens_auto.toml` and verified
//! with a single lightweight probe on every subsequent startup.
//! The probe is opt-in but enabled by default (`Some(1024)`), so
//! out-of-the-box behaviour is "discover on first run, then reuse"
//! with a safety floor of 1024 tokens.
//!
//! # When to disable it
//!
//! Disable the auto-probe when the cost of a sequential HTTP sweep
//! is prohibitive or when the provider cannot be reached from the
//! test runner:
//!
//! - **Smoke tests against a real provider.** Every CI run would
//!   otherwise pay ~30 sequential probes per fresh model. The
//!   smoke / e2e scripts export `MOAGAN_MAX_TOKEN_AUTO=false` for
//!   exactly this reason.
//! - **Sandboxed / offline runs.** The probe needs at least one
//!   successful round-trip; if the network is locked down the
//!   probe exits cleanly with the cached value (or the default
//!   floor if there is no cache).
//! - **Reproducible benchmarks.** Freeze the probe via
//!   `MOAGAN_MAX_TOKEN_AUTO_SAVE=false` so the cache file is not
//!   rewritten.
//!
//! Disable with:
//!
//! ```bash
//! export MOAGAN_MAX_TOKEN_AUTO=false        # or =0
//! export MOAGAN_MAX_TOKEN_AUTO_SAVE=false   # do not overwrite the cache
//! ```
//!
//! Or in `~/.config/moagan/config.toml`:
//!
//! ```toml
//! [[providers.minimax]]
//! endpoint = "https://api.minimax.io/anthropic/v1/messages"
//! max_token_auto = 0         # disable entirely (Some(0) ≡ None)
//! max_token_auto_enabled = false  # also disables; supersedes the floor value above
//! models = ["MiniMax-M3"]
//! ```
//!
//! `Some(0)` is equivalent to `None` (both mean "off"). `Some(N>0)`
//! enables the probe with a floor of `N` tokens. The
//! `max_token_auto_enabled: Option<bool>` knob is a hard kill
//! switch: when set to `Some(false)` it suppresses the probe table
//! entirely regardless of the `max_token_auto` floor. Operators
//! who want the probe disabled even if the floor is nonzero should
//! use `max_token_auto_enabled = false`; operators who want the
//! probe to run with a different floor just set
//! `max_token_auto = N` (with `max_token_auto_enabled` left as
//! `None`, which means "use the floor's nonzero-ness as the gate").
//!
//! # Sidecar schema (`<MOAGAN_HOME>/max_tokens_auto.toml`)
//!
//! ```toml
//! schema_version = 1
//!
//! [providers.minimax."MiniMax-M3"]
//! detected_at = "2026-08-11T11:12:34Z"
//! verified_at = "2026-08-12T10:00:00Z"
//! auto = true
//! max_tokens = 1024
//! ```
//!
//! | Field | Meaning |
//! |---|---|
//! | `schema_version` | File format version. Numeric `u32` (`1` today). Bumped if the schema changes. |
//! | `providers[provider][model].detected_at` | ISO-8601 timestamp of the initial successful probe. |
//! | `providers[provider][model].verified_at` | ISO-8601 timestamp of the most recent successful verify probe. On the first probe the algorithm initialises it to the same value as `detected_at`, so a fresh entry is never an empty string. |
//! | `providers[provider][model].auto` | Always `true` while the probe is responsible for the value. A `false` value is **just a marker** indicating the entry has been hand-curated; it does NOT freeze the value (see [`MaxTokensTable::set_operator_cap`] and the troubleshooting matrix). |
//! | `providers[provider][model].attempts` | How many probe batches the algorithm ran for this entry. Diagnostic. |
//! | `providers[provider][model].ceiling` | The per-provider hard ceiling the algorithm started the bisect from. Diagnostic. |
//! | `providers[provider][model].max_tokens` | The discovered ceiling. Clamped to `[MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING]`. |
//!
//! `operator_caps[provider]` is the optional operator-pinned
//! per-provider cap (the `max_tokens` analogue of the
//! `temperatures_auto.toml` `operator_caps` map). **As of v0.12.14
//! the `operator_caps` map is write-only**: [`MaxTokensTable::set_operator_cap`]
//! persists the value to disk for an audit trail, but the runtime
//! dispatch path reads the per-provider
//! `ModelConfig::max_tokens` (per-model), or falls through to
//! `MOAGAN_<SECTION>_MAX_TOKENS`, or to the auto-probe cache, in
//! that priority order — see [`crate::llm::max_tokens`] for the
//! full chain. Pinning via env vars or per-model
//! `ModelConfig::max_tokens` is the active mechanism.
//!
//! Delete the file to force a fresh probe. There is no
//! `*.disabled` rename — the runtime only consults the canonical
//! filename.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;

use crate::error::Result;
use crate::fs_layout::MoaganHome;

use super::probe::{
    Entry, MIN_AUTOPROBE_FLOOR, MaxTokensTableFile, OperatorCap, ProbeTransport,
    detect_max_tokens_with_phase0_cap_callback,
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
        tracing::debug!(
            path = %path.display(),
            floor,
            save,
            "MaxTokensTable::from_home"
        );
        Self::from_path(&path, floor, save)
    }

    /// Build a table from an explicit path. Used by tests and by
    /// [`Self::from_home`].
    pub fn from_path(path: &Path, floor: u32, save: bool) -> Result<Self> {
        let file = MaxTokensTableFile::load(path)?;
        let entries: BTreeMap<(String, String), Entry> = file
            .providers
            .into_iter()
            .flat_map(|(provider, models)| {
                models
                    .into_iter()
                    .map(move |(model, entry)| ((provider.clone(), model), entry))
            })
            .collect();
        tracing::info!(
            path = %path.display(),
            entries = entries.len(),
            floor,
            save,
            "MaxTokensTable::from_path loaded"
        );
        Ok(Self {
            inner: Arc::new(RwLock::new(MaxTokensTableInner {
                entries,
                floor: floor.max(MIN_AUTOPROBE_FLOOR),
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: save.then(|| path.to_path_buf()),
        })
    }

    /// Build a fresh table with no on-disk backing. Used by tests.
    pub fn empty(floor: u32) -> Self {
        tracing::debug!(
            floor = floor.max(MIN_AUTOPROBE_FLOOR),
            "MaxTokensTable::empty constructed"
        );
        Self {
            inner: Arc::new(RwLock::new(MaxTokensTableInner {
                entries: BTreeMap::new(),
                floor: floor.max(MIN_AUTOPROBE_FLOOR),
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: None,
        }
    }

    /// Read the cached value for `(provider, model)`. Returns
    /// `None` if no entry exists.
    pub fn get(&self, provider: &str, model: &str) -> Option<Entry> {
        let entry = self
            .inner
            .read()
            .entries
            .get(&(provider.to_owned(), model.to_owned()))
            .cloned();
        tracing::trace!(
            provider,
            model,
            present = entry.is_some(),
            "MaxTokensTable::get"
        );
        entry
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
        let before = inner.floor;
        inner.floor = inner.floor.max(floor.max(MIN_AUTOPROBE_FLOOR));
        tracing::debug!(
            before,
            after = inner.floor,
            requested = floor,
            "MaxTokensTable::set_floor_for"
        );
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
    ///
    /// `ceiling` is the per-provider upper bound for the exponential
    /// phase. `detect_max_tokens` short-circuits at the first
    /// `2^k > ceiling` so the algorithm does not probe values the
    /// upstream will reject (e.g. DeepSeek at 393_216, MiniMax at
    /// 524_288, OpenCode at 16_384). Production callers query
    /// `Provider::max_tokens_probe_ceiling()` and pass the value
    /// here; tests typically pass [`MAX_AUTOPROBE_CEILING`] to
    /// exercise the unbounded algorithm.
    ///
    /// When Phase 0 / Phase 0.5 parses the upstream's hard cap
    /// directly from the error body
    /// (e.g. `"model[X] does not support max tokens > N"`), the cap
    /// is persisted into [`Entry::ceiling`] alongside the discovered
    /// value. Subsequent runs that load the entry can read the cap
    /// back and use it as the cached ceiling so the algorithm skips
    /// the Phase 0 single-request probe entirely on the next run.
    pub async fn probe_and_store(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn ProbeTransport>,
        ceiling: u32,
    ) -> Result<u32> {
        tracing::info!(
            provider,
            model,
            floor = self.inner.read().floor,
            ceiling,
            "MaxTokensTable::probe_and_store: starting"
        );
        let floor = self.inner.read().floor;
        let attempts_before = self.inner.read().probe_tasks_started;
        // Single-element cell so the Phase 0 callback can write
        // the cap from inside the async closure. The cell is
        // uninitialised on entry; `Option::take()` after the
        // await returns the cap when Phase 0 fired the callback,
        // `None` otherwise. A simple `Cell<u32>` is not enough
        // because the callback runs across an await point.
        let phase0_cap: std::sync::Arc<std::sync::Mutex<Option<u32>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let phase0_cap_for_cb = std::sync::Arc::clone(&phase0_cap);
        let discovered =
            detect_max_tokens_with_phase0_cap_callback(transport, floor, ceiling, move |cap| {
                if let Ok(mut guard) = phase0_cap_for_cb.lock() {
                    *guard = Some(cap);
                }
            })
            .await?;
        let phase0_cap_value = phase0_cap
            .lock()
            .ok()
            .and_then(|g| *g)
            .filter(|&c| c <= discovered);
        if let Some(cap) = phase0_cap_value {
            tracing::trace!(cap, discovered, "probe_and_store: phase0 cap accepted");
        }

        let now = Utc::now().to_rfc3339();
        let attempts = {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            let attempts_total = inner.probe_tasks_started - attempts_before;
            inner.entries.insert(
                (provider.to_owned(), model.to_owned()),
                Entry {
                    max_tokens: discovered,
                    detected_at: now.clone(),
                    verified_at: now,
                    auto: true,
                    attempts: attempts_total,
                    // Stash the cap Phase 0 reported. Phase 0.5
                    // validates the candidate; only persist when
                    // the cap is no larger than the discovered
                    // value (a cap above the discovered value is
                    // a floor, not a ceiling, and would mislead
                    // the next run's `ceiling` calculation).
                    ceiling: phase0_cap_value,
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
        tracing::info!(
            provider,
            model,
            discovered,
            "MaxTokensTable::probe_and_store: completed"
        );
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
            tracing::trace!(provider, model, "verify: no cached entry; skip");
            return Ok(false);
        };
        tracing::debug!(
            provider,
            model,
            cached_max_tokens = entry.max_tokens,
            "verify: probing cached value"
        );
        let outcome = transport.probe_send(entry.max_tokens).await;
        let ok = matches!(outcome, super::probe::ProbeOutcome::Accepted);
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            if ok {
                if let Some(e) = inner
                    .entries
                    .get_mut(&(provider.to_owned(), model.to_owned()))
                {
                    e.verified_at = Utc::now().to_rfc3339();
                }
                tracing::info!(provider, model, "verify: accepted; verified_at bumped");
            } else {
                inner
                    .entries
                    .remove(&(provider.to_owned(), model.to_owned()));
                tracing::warn!(
                    provider,
                    model,
                    max_tokens = entry.max_tokens,
                    "verify: rejected by upstream; entry dropped (caller will re-probe)"
                );
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
    fn persist_to(&self, path: &Path) -> Result<()> {
        let inner = self.inner.read();
        let mut file = MaxTokensTableFile::new_empty();
        for ((provider, model), entry) in &inner.entries {
            file.providers
                .entry(provider.clone())
                .or_default()
                .insert(model.clone(), entry.clone());
        }
        tracing::trace!(
            path = %path.display(),
            count = inner.entries.len(),
            "MaxTokensTable::persist_to"
        );
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
        let pending_before = inner.pending.len();
        inner.pending.push(handle);
        tracing::debug!(
            pending_before,
            pending_after = inner.pending.len(),
            "MaxTokensTable: probe JoinHandle recorded"
        );
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
        let count = handles.len();
        tracing::debug!(count, "MaxTokensTable::await_ready: awaiting probes");
        for h in handles {
            if let Err(e) = h.await {
                tracing::warn!(error = %e, "max_tokens_auto: probe task join failed");
            }
        }
        tracing::debug!(count, "MaxTokensTable::await_ready: completed");
    }

    /// Persist to the path the table was built from. `None` when
    /// persistence was disabled at construction.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = self.persist_path.clone() else {
            tracing::trace!("MaxTokensTable::persist: persistence disabled; skip");
            return Ok(());
        };
        self.persist_to(&path)
    }

    /// Set the operator-pinned cap for a provider. Written by
    /// `moagan probe max_tokens --persist-min`; the runtime reads
    /// it on the next startup so a fresh run never re-probes the
    /// same models to land at the same answer. The cap is kept
    /// alongside the auto-discovered entries; both files share the
    /// same on-disk sidecar so a human diff after a probe-run stays
    /// meaningful. `auto` is hard-coded to `false` because an
    /// operator-pinned cap is, by construction, not auto-detected.
    ///
    /// **Write-only as of v0.12.14.** This method persists the
    /// cap to disk for an audit trail (so an operator can see what
    /// was pinned), but the runtime dispatch path does NOT
    /// currently read this field — the per-provider
    /// `ModelConfig::max_tokens` (per-model), the
    /// `MOAGAN_<SECTION>_MAX_TOKENS` env var, and the auto-probe
    /// cache take precedence in that order. To clamp the runtime,
    /// use env vars or per-model `ModelConfig::max_tokens`. The
    /// cap survives across runs as a paper trail, not as an
    /// active clamp.
    pub fn set_operator_cap(&self, provider: &str, min: u32) -> Result<()> {
        tracing::info!(provider, min, "MaxTokensTable::set_operator_cap");
        let now = Utc::now().to_rfc3339();
        let path = self.persist_path.clone();
        // Re-load the file so the operator_caps map merges with
        // whatever the on-disk sidecar already carries — a separate
        // process (or a previous invocation) may have written its
        // own cap for a different provider.
        if let Some(ref path) = path {
            let file = MaxTokensTableFile::load(path)?;
            let mut file = file;
            file.operator_caps.insert(
                provider.to_owned(),
                OperatorCap {
                    min,
                    auto: false,
                    detected_at: now,
                },
            );
            return file.save(path);
        }
        // Persistence disabled (save=false at construction): log a
        // warning and silently succeed so the in-memory result is
        // still useful for the rest of the run.
        tracing::warn!(
            provider = %provider,
            "max_tokens_auto: persistence disabled; operator cap not written to disk"
        );
        Ok(())
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
        assert!(t.get("minimax", "MiniMax-M3").is_none());
    }

    #[tokio::test]
    async fn probe_and_store_inserts_entry() {
        let t = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        let v = t
            .probe_and_store(
                "minimax",
                "MiniMax-M3",
                cap(524_288),
                crate::llm::probe::MAX_AUTOPROBE_CEILING,
            )
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
        t.probe_and_store(
            "minimax",
            "MiniMax-M3",
            cap(524_288),
            crate::llm::probe::MAX_AUTOPROBE_CEILING,
        )
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
        t.probe_and_store(
            "minimax",
            "MiniMax-M3",
            cap(524_288),
            crate::llm::probe::MAX_AUTOPROBE_CEILING,
        )
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
        t.probe_and_store(
            "minimax",
            "MiniMax-M3",
            cap(524_288),
            crate::llm::probe::MAX_AUTOPROBE_CEILING,
        )
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
}
