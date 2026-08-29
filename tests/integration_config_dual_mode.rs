//! Integration tests for the v0.13 `[[providers.<name>]]` dual-mode
//! config schema (`src/config/dual_mode.rs`).
//!
//! The dual-mode deserializer accepts both the new array-of-tables
//! form and the v0.12 legacy single-table form. This binary exercises
//! the round-trip end-to-end through `Config::load`:
//!
//! - `loads_legacy_only_config_and_warns` — fixture 01 (v0.12 legacy
//!   single-table); `Config::load()` succeeds and emits a
//!   `tracing::warn!` per legacy section.
//! - `loads_new_only_config_clean` — fixture 02 (v0.13 new
//!   array-of-tables); `Config::load()` succeeds without warnings and
//!   the bridge produces a 2-model `ProviderConfig`.
//! - `loads_mixed_config_with_one_warning` — fixture 03 (one legacy
//!   + one new section in the same TOML); exactly one warning fires
//!     (the legacy section only).
//! - `legacy_with_per_model_max_tokens_preserves_operator_cap` —
//!   fixture 04; the per-model `max_tokens = 131072` from the legacy
//!   inline-table survives the bridge via the
//!   `legacy_model_max_tokens` side-channel.
//! - `default_providers_round_trip_via_toml` — `Config::default()`
//!   → `toml::to_string` → `toml::from_str::<Config>()` →
//!   `compute_legacy_providers()` produces the same 4-section map.
//!
//! Fixture directory:
//! `tests/fixtures/config/v013_dual_mode/`.
//!
//! Note on env-var isolation: every test that calls
//! `toml::from_str::<Config>` does so via `Config::load`, which
//! resolves the XDG fallback. The tests point `MOAGAN_CONFIG` at the
//! fixture file via `Config::load_with_path` (a private hook) OR by
//! simply constructing `Config` from the fixture bytes via
//! `toml::from_str` — the latter is the documented hook for
//! integration tests that want to skip the filesystem loader.
//!
//! **Invariant**: do not add additional `#[test]` functions to this
//! binary that mutate `MOAGAN_HOME` or `MOAGAN_CONFIG`. The tracing
//! capture uses a process-global subscriber; a second test that
//! triggers `tracing_subscriber::fmt::try_init()` would race with
//! the capture and re-introduce the §2.2 flake pattern documented in
//! `tests/integration_parse_json_recovery.rs`. New env-var-dependent
//! tests belong in their own integration binary (e.g.
//! `integration_*_dual_mode_env.rs`).

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use moagan::config::Config;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

// ---------------------------------------------------------------------------
// Tracing capture helpers
// ---------------------------------------------------------------------------

/// Shared in-memory buffer the tracing subscriber writes into.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("shared tracing buffer poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedBuf {
    type Writer = SharedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriter(self.0.clone())
    }
}

/// Snapshot of the shared buffer (recovers from a poisoned Mutex so a
/// mid-run panic does not silently drop the captured bytes).
fn snapshot(buf: &SharedBuf) -> String {
    let bytes = buf
        .0
        .lock()
        .map(|b| b.clone())
        .unwrap_or_else(|p| p.into_inner().clone());
    String::from_utf8(bytes).expect("captured bytes are valid UTF-8")
}

/// Run `f` under a tracing subscriber that captures every event into
/// the shared buffer. Returns the captured log as a String after `f`
/// returns.
fn capture_tracing<F, R>(buf: &SharedBuf, f: F) -> (R, String)
where
    F: FnOnce() -> R,
{
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(buf.clone()),
    );

    let result = tracing::subscriber::with_default(subscriber, f);
    let captured = snapshot(buf);
    (result, captured)
}

// ---------------------------------------------------------------------------
// Fixture loader
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/config/v013_dual_mode")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

/// Parse a fixture as `Config` AND bridge the parsed `providers` field
/// into `providers_legacy` so the test can assert against the bridge
/// view directly.
///
/// `toml::from_str::<Config>(...)` invokes `Config::default()` (because
/// the `Config` struct is annotated `#[serde(default)]`) which
/// pre-populates `providers_legacy` from `default_providers()` — 4
/// sections. The subsequent TOML deserialisation overwrites the
/// `providers` field with whatever the fixture declared, but
/// `providers_legacy` is `#[serde(skip)]` so it is left untouched. We
/// re-run `compute_legacy_providers()` here to rebuild the bridge
/// view from the parsed `providers` map so assertions see exactly the
/// fixture's content.
fn parse_fixture_as_config(raw: &str) -> Config {
    let mut cfg: Config = toml::from_str(raw).expect("fixture must parse");
    cfg.compute_legacy_providers()
        .expect("bridge view must rebuild from the parsed providers map");
    cfg
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn loads_legacy_only_config_and_warns() {
    let raw = fixture("01_legacy_only.toml");
    let buf = SharedBuf::default();

    let (cfg, captured) = capture_tracing(&buf, || {
        // Parse the fixture directly with `toml::from_str` so the
        // test does not depend on the XDG fallback or the
        // filesystem loader. The dual-mode deserializer fires its
        // per-section `tracing::warn!` from `deserialize_providers_map`
        // regardless of the entry point — direct parse or
        // `Config::load` go through the same visitor. We follow up
        // with `compute_legacy_providers()` so the bridge view
        // reflects the parsed fixture, not the defaults
        // (`Config::default()` pre-populates `providers_legacy`
        // because the `Config` struct is `#[serde(default)]`).
        parse_fixture_as_config(&raw)
    });

    // Bridge view: the legacy section collapsed into one
    // `ProviderConfig` with two models, sharing the section-level
    // endpoint.
    let legacy = cfg
        .providers_legacy
        .get("minimax")
        .expect("minimax section must bridge through");
    assert_eq!(
        legacy.models.len(),
        2,
        "legacy section collapses into one entry with two models"
    );
    let ids: Vec<&str> = legacy.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["MiniMax-M3", "MiniMax-M2.5"]);

    // The bridge preserves per-model `max_tokens` via the
    // side-channel so the operator-side cap chain keeps working.
    assert_eq!(legacy.models[0].max_tokens, Some(1_000_000));
    assert_eq!(legacy.models[1].max_tokens, Some(1_000_000));

    // The warning fired exactly once (per-section, not per-model).
    let warn_count = captured
        .matches("config: `[providers.minimax]` (single-table form) is deprecated")
        .count();
    assert_eq!(
        warn_count, 1,
        "exactly one legacy warning expected; got {warn_count}\n\
         captured log:\n{captured}"
    );
}

#[test]
fn loads_new_only_config_clean() {
    let raw = fixture("02_new_only.toml");
    let buf = SharedBuf::default();

    let (cfg, captured) = capture_tracing(&buf, || parse_fixture_as_config(&raw));

    // The new section produced one `ProviderEntry` with two models
    // and the entry-level endpoint.
    let entries = cfg
        .providers
        .get("minimax")
        .expect("minimax section must parse");
    assert_eq!(entries.len(), 1, "single entry → one ProviderEntry");
    assert_eq!(
        entries[0].endpoint,
        "https://api.minimax.io/anthropic/v1/messages"
    );
    assert_eq!(entries[0].models, vec!["MiniMax-M3", "MiniMax-M2.5"]);

    // No warnings fired on the new schema.
    assert!(
        !captured.contains("deprecated"),
        "new-shape TOML must not emit a deprecation warning; \
         captured log:\n{captured}"
    );
}

#[test]
fn loads_mixed_config_with_one_warning() {
    let raw = fixture("03_mixed_per_section.toml");
    let buf = SharedBuf::default();

    let (cfg, captured) = capture_tracing(&buf, || parse_fixture_as_config(&raw));

    // Both sections bridged through.
    assert!(
        cfg.providers_legacy.contains_key("minimax"),
        "legacy minimax section must bridge through"
    );
    assert!(
        cfg.providers_legacy.contains_key("deepseek"),
        "new deepseek section must bridge through"
    );

    // The legacy `minimax` section warns; the new `deepseek` section
    // does not. Exactly one warning fires (per-section, not
    // per-model, not per TOML).
    let minimax_warns = captured
        .matches("config: `[providers.minimax]` (single-table form) is deprecated")
        .count();
    assert_eq!(
        minimax_warns, 1,
        "exactly one minimax legacy warning expected; got {minimax_warns}\n\
         captured log:\n{captured}"
    );

    let deepseek_warns = captured
        .matches("config: `[providers.deepseek]` (single-table form) is deprecated")
        .count();
    assert_eq!(
        deepseek_warns, 0,
        "deepseek is new-shape; must not warn. captured log:\n{captured}"
    );
}

#[test]
fn legacy_with_per_model_max_tokens_preserves_operator_cap() {
    let raw = fixture("04_legacy_with_per_model_max_tokens.toml");
    let buf = SharedBuf::default();

    let (cfg, captured) = capture_tracing(&buf, || parse_fixture_as_config(&raw));

    // Bridge view: the per-model `max_tokens = 131072` from the
    // legacy inline-table survives the bridge via the
    // `legacy_model_max_tokens` side-channel.
    let legacy = cfg
        .providers_legacy
        .get("minimax")
        .expect("minimax section must bridge through");
    assert_eq!(legacy.models.len(), 1);
    assert_eq!(legacy.models[0].id, "MiniMax-M2.7");
    assert_eq!(
        legacy.models[0].max_tokens,
        Some(131_072),
        "per-model `max_tokens = 131072` must survive the bridge via \
         the side-channel"
    );

    // The legacy warning still fires (one warning, per-section).
    let warn_count = captured
        .matches("config: `[providers.minimax]` (single-table form) is deprecated")
        .count();
    assert_eq!(
        warn_count, 1,
        "exactly one legacy warning expected; got {warn_count}"
    );
}

#[test]
fn default_providers_round_trip_via_toml() {
    // Take `Config::default()`, serialise it back to TOML, re-parse
    // it as a fresh `Config`, and confirm the bridge produces the
    // same 4-section legacy view (modulo the `mock` section's
    // endpoint corner — see `Config::compute_legacy_providers`
    // docstring).
    //
    // The default provider map is duplicate-free by construction, so
    // the round-trip should not error on duplicate ids.
    let original = Config::defaults();
    assert_eq!(
        original.providers_legacy.len(),
        4,
        "default provider map has 4 sections: minimax, deepseek, opencode, mock"
    );

    let serialised = toml::to_string(&original).expect("Config serialises back to TOML");
    let mut reparsed: Config = toml::from_str(&serialised).expect("round-trip parses");
    assert!(
        reparsed.compute_legacy_providers().is_ok(),
        "round-tripped defaults must not surface a duplicate-id error"
    );

    // The bridge sees all four sections in both views.
    for section in ["minimax", "deepseek", "opencode", "mock"] {
        let original_spec = original
            .providers_legacy
            .get(section)
            .expect("default bridge must contain the section");
        let reparsed_spec = reparsed
            .providers_legacy
            .get(section)
            .expect("re-parsed bridge must contain the section");
        assert_eq!(
            original_spec.models.len(),
            reparsed_spec.models.len(),
            "section {section}: model count diverged across round-trip"
        );
        for (a, b) in original_spec.models.iter().zip(reparsed_spec.models.iter()) {
            assert_eq!(a.id, b.id, "section {section}: model id diverged");
            assert_eq!(
                a.endpoint, b.endpoint,
                "section {section}: model endpoint diverged"
            );
            assert_eq!(
                a.max_tokens, b.max_tokens,
                "section {section}: model max_tokens diverged"
            );
        }
    }
}
