//! Centralised `max_tokens` resolver — v0.13.0 B-1 PR #3.
//!
//! Every LLM call goes through [`resolve_max_tokens`] to compute the
//! effective ceiling. The chain is explicit and operator-visible:
//!
//! 1. `MOAGAN_<SECTION>_MAX_TOKENS` env var (highest priority; lets
//!    an operator clamp a single section without touching TOML).
//! 2. `MaxTokensTable::get(section, model)` cache — the value the
//!    auto-probe discovered and persisted to
//!    `<MOAGAN_HOME>/max_tokens_auto.toml`. Filtered against
//!    [`MIN_AUTOPROBE_FLOOR`] so a hand-edited or corrupted sidecar
//!    cannot inject a degenerate value (plan §7.B #27).
//! 3. `operator_cap` → `kind_hard_cap` → [`DEFAULT_MAX_TOKENS`]
//!    (1,000,000). Each `None` falls through to the next; the last
//!    rung is always present.
//!
//! The helper never panics: NUL bytes (returns `Err(InvalidInput)`),
//! unparseable values, whitespace-padded values, BOM-prefixed values,
//! and out-of-range values all fall through to the next rung with a
//! `tracing::warn!` or `trace!` so the operator sees the degradation
//! in the structured log.
//!
//! # Returns
//!
//! A `u32` in `[\"MIN_AUTOPROBE_FLOOR\", MAX_AUTOPROBE_CEILING]`.
//! Call-sites that need an additional per-provider safety cap
//! (e.g. `MINIMAX_MAX_TOKENS_CAP`) apply `.min(cap)` **after** the
//! helper, not before — otherwise the helper's ceiling would be
//! silently shadowed by a layer that doesn't know about env/cached
//! overrides.

use super::probe::{MAX_AUTOPROBE_CEILING, MIN_AUTOPROBE_FLOOR};
use super::probe_table::MaxTokensTable;
use super::prompts::DEFAULT_MAX_TOKENS;

/// Resolve the `max_tokens` ceiling through the chain
/// `env → cached auto-probe → operator_cap → kind_hard_cap → DEFAULT_MAX_TOKENS`.
///
/// See the module-level docs for the full precedence rules and the
/// "never panic" invariants.
///
/// The function does NOT log at the table-hit rung by default — a
/// pipeline at 1 Hz would otherwise emit a `trace` event per call,
/// bloating the log. Operators wanting per-call visibility can attach
/// their own subscriber with a lower filter level.
pub fn resolve_max_tokens(
    section: &str,
    model: &str,
    table: Option<&MaxTokensTable>,
    operator_cap: Option<u32>,
    kind_hard_cap: Option<u32>,
) -> u32 {
    // 1. Env var override. Section name normalised so that
    //    `MOAGAN_OPENCODE_GO_MAX_TOKENS` works regardless of whether
    //    the operator named the section `opencode-go`,
    //    `opencode.go`, or `opencode_go`.
    let env_key = format!(
        "MOAGAN_{}_MAX_TOKENS",
        section.to_uppercase().replace(['.', '-'], "_")
    );
    match std::env::var(&env_key) {
        Ok(raw) => {
            // BOM-prefixed values (Windows-saved shell exports) and
            // surrounding whitespace both slip through `trim()`
            // because `'\u{FEFF}'` is in Unicode category `Cf`, not
            // whitespace. Strip explicitly before parse.
            let cleaned = raw.trim_start_matches('\u{FEFF}').trim();
            match cleaned.parse::<u32>() {
                Ok(n) if (MIN_AUTOPROBE_FLOOR..=MAX_AUTOPROBE_CEILING).contains(&n) => {
                    tracing::debug!(
                        env = env_key.as_str(),
                        value = n,
                        "resolve_max_tokens: env var wins"
                    );
                    return n;
                }
                Ok(n) => {
                    tracing::warn!(
                        env = env_key.as_str(),
                        value = n,
                        floor = MIN_AUTOPROBE_FLOOR,
                        ceiling = MAX_AUTOPROBE_CEILING,
                        "MOAGAN_<SECTION>_MAX_TOKENS out of range; falling through"
                    );
                }
                Err(_) if cleaned.is_empty() => {
                    // Empty / whitespace-only / BOM-only values fall
                    // through silently — the operator didn't ask for
                    // anything, so don't spam the log.
                    tracing::trace!(
                        env = env_key.as_str(),
                        "resolve_max_tokens: env var empty after trim; falling through"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        env = env_key.as_str(),
                        value = %cleaned,
                        "MOAGAN_<SECTION>_MAX_TOKENS unparseable; falling through"
                    );
                }
            }
        }
        // `Err(NotPresent)` — the operator didn't set the env var.
        // `Err(InvalidInput)` — NUL byte or other invalid content on
        // Unix. Both fall through without a log line at the hot
        // path's default filter level; a `trace` event is enough so
        // a debugging session can spot the cause.
        Err(e) => {
            tracing::trace!(
                env = env_key.as_str(),
                error = %e,
                "resolve_max_tokens: env var absent or invalid; falling through"
            );
        }
    }
    // 2. Cached auto-probe. Filter entries below the floor so a
    //    manually-edited sidecar (or a corrupted write from a
    //    concurrent run) cannot leak a degenerate value into the
    //    wire body.
    if let Some(t) = table
        && let Some(entry) = t.get(section, model)
        && entry.max_tokens >= MIN_AUTOPROBE_FLOOR
    {
        tracing::trace!(
            section,
            model,
            cached = entry.max_tokens,
            "resolve_max_tokens: cache wins"
        );
        return entry.max_tokens;
    }
    // 3. Operator TOML override (the per-model `max_token_auto`
    //    knob) and the kind-level cap (MiniMax upstream rejects
    //    > 524_288). When both are `Some`, the smaller wins — this
    //    is the same `min()` semantics the original hand-rolled
    //    chains at every migrated call-site used, and the audit-log
    //    hash contract requires the helper to return the same value
    //    `send()` would have applied. `None` on either side lets
    //    the other carry the chain; both `None` falls through to
    //    [`DEFAULT_MAX_TOKENS`].
    let out = match (operator_cap, kind_hard_cap) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => DEFAULT_MAX_TOKENS,
    };
    tracing::debug!(
        section,
        model,
        fallback = out,
        "resolve_max_tokens: layered fallback"
    );
    out
}

/// Thin wrapper for call-sites that don't carry a per-provider
/// `operator_cap` or `kind_hard_cap` (currently the OpenAI-compat
/// paths that pull `max_tokens` from a per-model `ModelConfig`
/// and apply no kind-level ceiling). Delegates to
/// [`resolve_max_tokens`] with `None, None` so the env/cache/fallback
/// chain still applies.
///
/// Kept as a separate function rather than a default-argument call
/// so the production call-sites read as
/// `resolve_max_tokens_simple(section, model, table)` — a future
/// reviewer grep'ing for `resolve_max_tokens(` lands on every site
/// that consults the helper, including the simple ones.
pub fn resolve_max_tokens_simple(
    section: &str,
    model: &str,
    table: Option<&MaxTokensTable>,
) -> u32 {
    resolve_max_tokens(section, model, table, None, None)
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`resolve_max_tokens`].
    //!
    //! All tests serialise on a private mutex so they cannot race
    //! each other on the same `MOAGAN_*` env var. The crate-wide
    //! `TEST_ENV_LOCK` in `src/config/mod.rs` lives in a different
    //! module and would otherwise require a cross-file `pub`; a
    //! local lock keeps the surface tight.

    use std::sync::{Arc, Mutex};

    use crate::llm::probe::MIN_AUTOPROBE_FLOOR;
    use crate::llm::probe_table::MaxTokensTable;

    use super::{MAX_AUTOPROBE_CEILING, resolve_max_tokens, resolve_max_tokens_simple};

    /// Per-process mutex so tests that mutate `MOAGAN_*` env vars
    /// do not race. Same pattern as `TEST_ENV_LOCK` in
    /// `src/config/mod.rs:4169`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that removes the named env var on drop. Backs
    /// every test so a panic mid-test cannot leak the var into the
    /// next test's environment.
    struct EnvGuard {
        key: String,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn new(key: &str) -> Self {
            // SAFETY: the per-process mutex above serialises the
            // mutation; the guard restores the prior value on drop.
            let prior = std::env::var(key).ok();
            Self {
                key: key.to_owned(),
                prior,
            }
        }

        fn set(&self, value: &str) {
            // SAFETY: see `new`.
            unsafe { std::env::set_var(&self.key, value) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `new`. If the prior value was set, restore
            // it; otherwise remove the var so the next test starts
            // from a clean slate.
            unsafe {
                match &self.prior {
                    Some(v) => std::env::set_var(&self.key, v),
                    None => std::env::remove_var(&self.key),
                }
            }
        }
    }

    /// Helper: build a `MaxTokensTable` with one cached entry so
    /// the test can drive the cache-win branch without going
    /// through the async probe.
    fn table_with_entry(provider: &str, model: &str, max_tokens: u32) -> Arc<MaxTokensTable> {
        let table = MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR);
        // `MaxTokensTable::empty` returns an in-memory table; the
        // helper writes the entry through the public `get`-side
        // accessor the test then reads. The table exposes no public
        // `insert` because the only legitimate writer is the
        // auto-probe (via `probe_and_store`), so we exercise that
        // path with an `Accepted`-only transport for a single
        // request.
        let transport: Arc<dyn crate::llm::probe::ProbeTransport> =
            Arc::new(StaticTransport { cap: max_tokens });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let discovered =
            rt.block_on(table.probe_and_store(provider, model, transport, MAX_AUTOPROBE_CEILING));
        assert_eq!(
            discovered.expect("probe_and_store"),
            max_tokens,
            "table fixture helper must converge to the requested value"
        );
        Arc::new(table)
    }

    /// Trivial `ProbeTransport` whose `probe_send` accepts every
    /// value up to `cap` and rejects anything above. Used by
    /// `table_with_entry` to inject a known cache entry without
    /// firing real HTTP.
    struct StaticTransport {
        cap: u32,
    }

    #[async_trait::async_trait]
    impl crate::llm::probe::ProbeTransport for StaticTransport {
        async fn probe_send(&self, n: u32) -> crate::llm::probe::ProbeOutcome {
            if n <= self.cap {
                crate::llm::probe::ProbeOutcome::Accepted
            } else {
                crate::llm::probe::ProbeOutcome::Rejected
            }
        }
    }

    // ---- env-over-cache precedence (plan §6.2) -------------------

    #[test]
    fn env_overrides_cache() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("99999");
        let table = table_with_entry("minimax", "MiniMax-M3", 4096);
        let got = resolve_max_tokens("minimax", "MiniMax-M3", Some(&table), None, None);
        assert_eq!(got, 99_999, "env var must beat cached value");
    }

    #[test]
    fn cache_wins_when_no_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set(""); // ensure absent after trim
        let table = table_with_entry("minimax", "MiniMax-M3", 4096);
        let got = resolve_max_tokens("minimax", "MiniMax-M3", Some(&table), None, None);
        assert_eq!(got, 4096, "cache must win when no env var is set");
    }

    #[test]
    fn fallback_chain_when_no_cache_no_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        // No cache, no env, no caps -> DEFAULT_MAX_TOKENS.
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(
            got,
            crate::llm::prompts::DEFAULT_MAX_TOKENS,
            "fallback must use DEFAULT_MAX_TOKENS when every rung above is absent"
        );
    }

    #[test]
    fn kind_hard_cap_wins_over_operator_cap() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, Some(99_999), Some(1024));
        assert_eq!(
            got, 1024,
            "smaller cap (kind_hard_cap=1024) must beat larger operator_cap=99_999"
        );
    }

    #[test]
    fn operator_cap_wins_over_kind_hard_cap() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, Some(1024), Some(99_999));
        assert_eq!(
            got, 1024,
            "smaller cap (operator_cap=1024) must beat larger kind_hard_cap=99_999"
        );
    }

    // ---- env validation (plan §6.2 / §7.B #22, #24, #25, #27) ----

    #[test]
    fn env_zero_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("0");
        // No cache, no caps -> DEFAULT_MAX_TOKENS (1M).
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(
            got,
            crate::llm::prompts::DEFAULT_MAX_TOKENS,
            "env=0 must fall through (below MIN_AUTOPROBE_FLOOR)"
        );
    }

    #[test]
    fn env_out_of_range_warns_and_falls_through() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("10");
        // Below floor -> falls through to DEFAULT_MAX_TOKENS.
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(got, crate::llm::prompts::DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn env_above_ceiling_warns_and_falls_through() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        // Above MAX_AUTOPROBE_CEILING (2^30).
        guard.set("99999999999");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(got, crate::llm::prompts::DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn env_invalid_value_warns_and_falls_through() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("abc");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(got, crate::llm::prompts::DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn env_trims_whitespace() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set(" 524288 ");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(got, 524_288, "surrounding whitespace must be trimmed");
    }

    #[test]
    fn env_bom_normalized() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        // BOM-prefixed value, a common Windows-saved shell export.
        guard.set("\u{FEFF}524288");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(got, 524_288, "leading BOM must be stripped before parse");
    }

    #[test]
    fn section_uppercased_in_env_lookup() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("4096");
        // Caller passes the lowercased section name; helper must
        // uppercase it before the env lookup.
        let got = resolve_max_tokens("minimax", "MiniMax-M3", None, None, None);
        assert_eq!(
            got, 4096,
            "MOAGAN_MINIMAX_MAX_TOKENS must be honoured for the \"minimax\" section"
        );
    }

    #[test]
    fn section_dots_and_dashes_normalized() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_OPENCODE_GO_MAX_TOKENS");
        guard.set("8192");
        // Caller passes `opencode-go`; helper must uppercase +
        // dashify so the lookup lands on `MOAGAN_OPENCODE_GO_MAX_TOKENS`.
        let got = resolve_max_tokens("opencode-go", "kimi-k3", None, None, None);
        assert_eq!(
            got, 8192,
            "dashes must be replaced with underscores in the env lookup"
        );
    }

    // ---- cache corruption guard (plan §7.B #27) ------------------

    #[test]
    fn cache_corrupt_value_below_floor_is_rejected() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        // Build a corrupt cache entry whose `max_tokens` is below
        // the floor. `probe_and_store` itself refuses to record
        // such a value (the discovery algorithm enforces the
        // floor), so the only way to land one in the table is to
        // hand-edit the on-disk sidecar — exactly the corruption
        // scenario the helper guards against. Build the
        // `MaxTokensTableFile` programmatically with a sub-floor
        // `max_tokens`, persist it via the public `save()` (which
        // also runs `quote_provider_model_keys`, so the on-disk
        // shape matches what the live probe writes), and load it
        // through `from_path`. The file loader preserves whatever
        // the file says; the helper's
        // `entry.max_tokens >= MIN_AUTOPROBE_FLOOR` filter is what
        // catches it on the read side.
        use crate::llm::probe::{Entry, MaxTokensTableFile};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("max_tokens_auto.toml");
        let mut file = MaxTokensTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    max_tokens: 512,
                    detected_at: "2026-08-29T00:00:00Z".to_owned(),
                    verified_at: "2026-08-29T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 0,
                    ceiling: None,
                },
            );
        file.save(&path).expect("save corrupt fixture");
        let table = MaxTokensTable::from_path(&path, MIN_AUTOPROBE_FLOOR, false)
            .expect("from_path must accept the corrupt fixture");
        let got = resolve_max_tokens("minimax", "MiniMax-M3", Some(&table), None, None);
        assert_eq!(
            got,
            crate::llm::prompts::DEFAULT_MAX_TOKENS,
            "cache entry below MIN_AUTOPROBE_FLOOR must be rejected"
        );
    }

    #[test]
    fn minimax_kind_cap_clamped_at_end() {
        // Plan §6.2 contract: the kind cap (`MINIMAX_MAX_TOKENS_CAP`
        // = 524_288) is the last rung in the chain. With a generous
        // operator cap and no cache the helper must surface the
        // kind cap. The call-site then applies `.min(kind_cap)` on
        // top of the helper's return value to enforce the
        // upstream-facing ceiling — that double-application is
        // intentional and required for the byte-identical
        // audit-log hash contract.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        let got = resolve_max_tokens(
            "minimax",
            "MiniMax-M3",
            None,
            Some(999_999),
            Some(crate::llm::capabilities::MINIMAX_MAX_TOKENS_CAP),
        );
        assert_eq!(got, crate::llm::capabilities::MINIMAX_MAX_TOKENS_CAP);
    }

    // ---- byte-identical contract between send() and effective_max_tokens() --

    #[test]
    fn send_and_effective_max_tokens_agree_on_minimax() {
        // Audit-log hash contract: `effective_max_tokens(req)` and
        // the value the wire body carries must be byte-identical.
        // Both code paths call the helper with the same arguments;
        // this test pins that the helper itself is deterministic
        // across a representative sample of inputs.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        let table = table_with_entry("minimax", "MiniMax-M3", 4096);
        let sent = resolve_max_tokens(
            "minimax",
            "MiniMax-M3",
            Some(&table),
            Some(crate::llm::capabilities::MINIMAX_MAX_TOKENS_CAP),
            Some(crate::llm::capabilities::MINIMAX_MAX_TOKENS_CAP),
        );
        let effective = resolve_max_tokens(
            "minimax",
            "MiniMax-M3",
            Some(&table),
            Some(crate::llm::capabilities::MINIMAX_MAX_TOKENS_CAP),
            Some(crate::llm::capabilities::MINIMAX_MAX_TOKENS_CAP),
        );
        assert_eq!(
            sent, effective,
            "send() and effective_max_tokens() must compute the same value via the helper"
        );
        assert_eq!(
            sent, 4096,
            "cache=4096 must win when env is absent and operator_cap=kind_cap=524_288"
        );
    }

    // ---- simple wrapper smoke (call-sites that have no caps) ----

    #[test]
    fn resolve_max_tokens_simple_delegates() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let guard = EnvGuard::new("MOAGAN_MINIMAX_MAX_TOKENS");
        guard.set("");
        let table = table_with_entry("minimax", "MiniMax-M3", 4096);
        let via_simple = resolve_max_tokens_simple("minimax", "MiniMax-M3", Some(&table));
        let via_full = resolve_max_tokens("minimax", "MiniMax-M3", Some(&table), None, None);
        assert_eq!(
            via_simple, via_full,
            "simple wrapper must delegate to the full helper"
        );
        assert_eq!(via_simple, 4096);
    }
}
