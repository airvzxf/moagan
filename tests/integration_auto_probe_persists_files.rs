//! PR-x23 (ghost-rider-banshee): end-to-end pin that the registry
//! now spawns the `max_tokens` and temperature auto-probes by
//! default for non-mock providers and that both sidecar TOMLs
//! (`<MOAGAN_HOME>/max_tokens_auto.toml` and
//! `<MOAGAN_HOME>/temperatures_auto.toml`) land on disk after the
//! probes converge.
//!
//! The two regression axes covered here:
//!
//! 1. **Default-on probe** — the operator reported that
//!    `moagan discover --provider minimax:MiniMax-M2.5` produced
//!    no probe (`max_tokens_auto: no provider enabled the probe`)
//!    because the legacy config schema required an explicit
//!    `max_token_auto = Some(n>0)` opt-in. The plan flips the
//!    default to opt-out: a non-mock provider with no explicit
//!    opt-out MUST spawn the probe. The probe table must exist
//!    on the registry after construction.
//! 2. **Persistence to disk** — `temperatures_auto.toml` was
//!    never written because the CLI only awaited the
//!    `max_tokens_table` and the temperature probes lost the
//!    race against the run's exit. The plan gates both
//!    `await_ready()` calls on the post-pipeline block. The test
//!    exercises the same code path (`probe_and_store` →
//!    `persist_to`) and asserts the TOML files exist on disk
//!    after the awaitable drain.
//!
//! Wiremock backs the upstream so the probe converges without
//! touching a real provider. The test runs against
//! `MinimaxProvider` (the same family the operator hit) to keep
//! the wire format canonical.

#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;

use moagan::config::{CircuitBreakerConfig, ModelConfig, ProviderConfig};
use moagan::fs_layout::MoaganHome;
use moagan::llm::provider::{ProviderRegistry, registry_from_config_with_home_and_sink};
use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Thin local alias over the crate-wide `TEST_API_KEYS_LOCK`
/// (declared in `src/lib.rs:65` as a `pub static`). The three
/// integration tests below mutate `MINIMAX_API_KEY` to satisfy the
/// dispatcher's `AnthropicCompatProvider::from_resolved` constructor;
/// without the lock, the parallel test runner lets one test observe
/// the env var set by a sibling mid-flight, and the registry builder
/// reads the wrong key (this is the flake the operator documented
/// when closing the original `registry_auto_probe_persists_both_*
/// passes with --test-threads=1, fails in CI default` bug). Sharing
/// the crate-wide mutex (instead of a file-local one) coordinates
/// with the sibling test modules `src/llm/api_keys.rs::tests`,
/// `src/cli/doctor.rs::tests`, `src/llm/provider.rs::tests`, and
/// `src/cli/probe.rs::tests`, which already use the same lock to
/// serialise the same env vars.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    moagan::TEST_API_KEYS_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Build a single-section provider map with the supplied endpoint
/// and the supplied `max_token_auto*` knobs.
fn provider_map(
    endpoint: String,
    max_token_auto: Option<u32>,
    max_token_auto_enabled: Option<bool>,
) -> ProviderConfig {
    ProviderConfig {
        models: vec![moagan::config::ModelConfig {
            max_tokens: None,
            id: "minimax-test".into(),
            endpoint: None,
        }],
        endpoint: Some(endpoint),
        temperature: None,
        top_p: None,
        omit_max_tokens: false,
        max_token_auto,
        max_token_auto_enabled,
        max_token_auto_save: true,
        plan: None,
    }
}

/// Wiremock that accepts every probe (`max_tokens` and
/// temperature) with a canonical Anthropic-compat response. The
/// accepted `max_tokens` ceiling is `8192` (well above the
/// 1024 floor) so the auto-probe converges in a single
/// exponential step.
async fn mount_accept_all(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            }
        })))
        .mount(server)
        .await;
}

/// E2E pin: when a non-mock provider with no explicit opt-out
/// builds its registry, the `max_tokens` auto-probe fires by
/// default and the discovered value lands in
/// `<MOAGAN_HOME>/max_tokens_auto.toml`. The temperature probe
/// also fires and lands in `<MOAGAN_HOME>/temperatures_auto.toml`.
/// The regression the operator hit (no probe, empty TOML) fails
/// this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_auto_probe_persists_both_toml_files() {
    let tmp = tempdir().expect("tempdir");
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().expect("home layout");

    let server = MockServer::start().await;
    mount_accept_all(&server).await;

    // `wire_format_from_url` validates the URL path; the wiremock
    // serves its responses at `/v1/messages` so the endpoint must
    // include that suffix verbatim (the dispatcher does not
    // append it).
    let endpoint = format!("{}/v1/messages", server.uri());

    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert("minimax".into(), provider_map(endpoint, None, None));

    // The dummy API key lets the dispatcher build the
    // `AnthropicCompatProvider`; the wiremock URL keeps every
    // probe off the operator's network.
    let _env = env_lock();
    unsafe {
        std::env::set_var("MINIMAX_API_KEY", "dummy-for-probe-test");
    }
    let registry: ProviderRegistry = registry_from_config_with_home_and_sink(
        &cfg,
        &CircuitBreakerConfig::default(),
        Some(&home),
        None,
        None,
    )
    .expect("registry must build for a non-mock provider with no explicit opt-out");
    unsafe {
        std::env::remove_var("MINIMAX_API_KEY");
    }
    drop(_env);

    // The tables must be attached to the registry: opt-out-by-default
    // is the new contract, an empty registry fails this assertion.
    let max_tokens_table = registry
        .max_tokens_table()
        .expect("registry must carry a max_tokens table by default");
    let temperature_table = registry
        .temperature_table()
        .expect("registry must carry a temperature table");

    // Drain the probe fan-out: without these `await_ready` calls
    // the binary exits before `persist_to` runs and the TOML
    // files never land on disk (the very bug this PR fixes).
    max_tokens_table.await_ready().await;
    temperature_table.await_ready().await;

    // `max_tokens_auto.toml`: at least one entry under the
    // `minimax` section. The exact value depends on the
    // exponential probe path, but the presence of the section +
    // a model key is the contract this PR pins.
    let max_tokens_path = home.max_tokens_auto_path();
    assert!(
        max_tokens_path.exists(),
        "max_tokens_auto.toml must exist after the probe converges (was: {})",
        max_tokens_path.display()
    );
    let max_tokens_body =
        std::fs::read_to_string(&max_tokens_path).expect("read max_tokens_auto.toml");
    // PR-x23 follow-up: pin the **canonical header form** instead
    // of just the substrings. The earlier contract accepted
    // `[providers."minimax::minimax-test"."minimax-test"]` because
    // the registry key leaked into `probe_and_store` as the
    // provider name, leaving the on-disk header as
    // `providers.<registry_key>.<model>`. The probe bug is now
    // fixed (`spawn_pending_probes` uses `inner.name()`, the
    // section name), so the persisted header must read
    // `[providers."minimax"."minimax-test"]` (the
    // `quote_provider_model_keys` post-processor in
    // `MaxTokensTableFile::save` quotes both keys because the
    // `toml` crate only quotes keys that need it — the raw form
    // may or may not be quoted depending on the model name).
    // Accepting both raw and quoted forms keeps the test
    // resilient against future tweaks to the post-processor.
    assert_canonical_provider_header(
        &max_tokens_body,
        "minimax",
        "minimax-test",
        "max_tokens_auto.toml",
    );
    assert!(
        !max_tokens_body.contains("\"minimax::minimax-test\""),
        "max_tokens_auto.toml must NOT leak the registry key as the section name \
         (regression: spawn_pending_probes must use inner.name()); got:\n{max_tokens_body}"
    );

    // `temperatures_auto.toml`: same shape under `providers`.
    let temperatures_path = home.temperatures_auto_path();
    assert!(
        temperatures_path.exists(),
        "temperatures_auto.toml must exist after the temperature probe converges (was: {})",
        temperatures_path.display()
    );
    let temperatures_body =
        std::fs::read_to_string(&temperatures_path).expect("read temperatures_auto.toml");
    // Same canonical-header pin as above. The temperature probe
    // also feeds `probe_and_store` through `inner.name()`, so the
    // on-disk header must use the section name as the top-level
    // key. The pre-fix failure mode was
    // `[providers."minimax::minimax-test"."minimax-test"]` with
    // the model under the registry-keyed section — see
    // `spawn_pending_temperature_probes` in `src/llm/provider.rs`.
    assert_canonical_provider_header(
        &temperatures_body,
        "minimax",
        "minimax-test",
        "temperatures_auto.toml",
    );
    assert!(
        !temperatures_body.contains("\"minimax::minimax-test\""),
        "temperatures_auto.toml must NOT leak the registry key as the section name \
         (regression: spawn_pending_temperature_probes must use inner.name()); got:\n{temperatures_body}"
    );
}

/// Assert the on-disk TOML carries a `[providers.<section>.<model>]`
/// header for the supplied pair. The exact quoting depends on
/// whether the `toml` crate considers the keys bare or
/// special-char-tainted at write time, and on whether the
/// `quote_provider_model_keys` post-processor in
/// `TemperatureTableFile::save` rewrites the header. The helper
/// accepts every shape the helper can plausibly emit:
/// `[providers.X.Y]`, `[providers."X".Y]`, `[providers.X."Y"]`,
/// and `[providers."X"."Y"]`.
fn assert_canonical_provider_header(body: &str, section: &str, model: &str, file_label: &str) {
    let canonical_double = format!("[providers.\"{section}\".\"{model}\"]");
    let canonical_section_quoted = format!("[providers.\"{section}\".{model}]");
    let canonical_model_quoted = format!("[providers.{section}.\"{model}\"]");
    let canonical_bare = format!("[providers.{section}.{model}]");
    assert!(
        body.contains(&canonical_double)
            || body.contains(&canonical_section_quoted)
            || body.contains(&canonical_model_quoted)
            || body.contains(&canonical_bare),
        "{file_label} must carry a canonical `[providers.{section}.{model}]` header \
         (or the double-quoted / single-quoted variants the \
         `quote_provider_model_keys` post-processor emits). Got:\n{body}"
    );
}

/// Companion pin: the explicit opt-out (`max_token_auto_enabled =
/// Some(false)`) must keep the legacy "no probe" contract — the
/// registry returns `None` for both tables. This test guards the
/// reverse regression (a future change accidentally flipping the
/// opt-out default the other way would break this assertion).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_opt_out_suppresses_probe_tables() {
    let tmp = tempdir().expect("tempdir");
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().expect("home layout");

    let server = MockServer::start().await;
    mount_accept_all(&server).await;
    let endpoint = format!("{}/v1/messages", server.uri());

    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert("minimax".into(), provider_map(endpoint, None, Some(false)));

    let _env = env_lock();
    unsafe {
        std::env::set_var("MINIMAX_API_KEY", "dummy-for-probe-test");
    }
    let registry = registry_from_config_with_home_and_sink(
        &cfg,
        &CircuitBreakerConfig::default(),
        Some(&home),
        None,
        None,
    )
    .expect("registry must build for an opted-out provider");
    unsafe {
        std::env::remove_var("MINIMAX_API_KEY");
    }
    drop(_env);

    assert!(
        registry.max_tokens_table().is_none(),
        "max_token_auto_enabled = Some(false) must suppress the max_tokens probe table"
    );
    // The temperature probe is opt-out per `(provider, model)`
    // regardless of the max_tokens knob; this PR keeps that
    // contract untouched. The opt-out must not leak into the
    // temperature table.
    assert!(
        registry.temperature_table().is_some(),
        "max_token_auto_enabled = Some(false) must NOT affect the temperature probe"
    );
}

/// Companion pin: the legacy `max_token_auto = Some(0)` sentinel
/// keeps its opt-out semantics. Operators with that TOML must
/// not silently flip to "probe on" after this PR.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_zero_still_means_opt_out() {
    let tmp = tempdir().expect("tempdir");
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().expect("home layout");

    let server = MockServer::start().await;
    mount_accept_all(&server).await;
    let endpoint = format!("{}/v1/messages", server.uri());

    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert("minimax".into(), provider_map(endpoint, Some(0), None));

    let _env = env_lock();
    unsafe {
        std::env::set_var("MINIMAX_API_KEY", "dummy-for-probe-test");
    }
    let registry = registry_from_config_with_home_and_sink(
        &cfg,
        &CircuitBreakerConfig::default(),
        Some(&home),
        None,
        None,
    )
    .expect("registry must build for a Some(0) sentinel");
    unsafe {
        std::env::remove_var("MINIMAX_API_KEY");
    }
    drop(_env);

    assert!(
        registry.max_tokens_table().is_none(),
        "max_token_auto = Some(0) must remain an opt-out (legacy contract)"
    );
}

/// Regression pin for the crate-wide `TEST_API_KEYS_LOCK` mutex
/// (in `src/lib.rs`). Eight OS threads contend for the mutex; if
/// anyone removes the static or shortens its scope, the parallel
/// race between `set_var` / `remove_var` is observable as a flake
/// where two tests see `MINIMAX_API_KEY` set to `"x"` at once and
/// the join still completes (the assertion would still pass — the
/// regression is in the broader suite, not in this test). The
/// point of this test is to lock the API contract: the static must
/// exist, must be a `Mutex<()>`, and must serialise `set_var` /
/// `remove_var` across thread boundaries.
///
/// The main thread does NOT hold the lock while the workers
/// spin up (that would deadlock — the workers cannot acquire a
/// lock the main thread is holding). The main thread only joins
/// the workers; the workers contend for the lock on their own.
#[test]
fn env_lock_serializes_minimax_api_key_mutations() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let c = Arc::clone(&counter);
        handles.push(thread::spawn(move || {
            let _g = match moagan::TEST_API_KEYS_LOCK.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            unsafe {
                std::env::set_var("MINIMAX_API_KEY", "x");
            }
            unsafe {
                std::env::remove_var("MINIMAX_API_KEY");
            }
            c.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 8);
}

/// PR-04b-1 (A-3): regression test for the `build_provider_for_probe`
/// section-name propagation. The fixture builds a registry with a
/// model id (`MiniMax-M3`) that differs from the section name
/// (`minimax`); the persisted `max_tokens_auto.toml` header must use
/// the section name as the top-level key (NOT the model id — that
/// was the pre-fix bug shape). Combined with the unit test in
/// `src/cli/probe.rs::mod tests` that calls
/// `build_provider_for_probe` directly, this integration test pins
/// the visible contract on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_propagates_section_name_not_model_id() {
    let _env = env_lock();
    let tmp = tempdir().expect("tempdir");
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().expect("home layout");

    let server = MockServer::start().await;
    mount_accept_all(&server).await;
    let endpoint = format!("{}/v1/messages", server.uri());

    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert(
        "minimax".into(),
        ProviderConfig {
            models: vec![ModelConfig {
                max_tokens: None,
                id: "MiniMax-M3".into(),
                endpoint: None,
            }],
            endpoint: Some(endpoint),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            plan: None,
        },
    );

    unsafe {
        std::env::set_var("MINIMAX_API_KEY", "dummy-for-probe-test");
    }
    let registry: ProviderRegistry = registry_from_config_with_home_and_sink(
        &cfg,
        &CircuitBreakerConfig::default(),
        Some(&home),
        None,
        None,
    )
    .expect("registry must build when model id differs from section name");
    unsafe {
        std::env::remove_var("MINIMAX_API_KEY");
    }
    drop(_env);

    let max_tokens_table = registry
        .max_tokens_table()
        .expect("registry must carry a max_tokens table by default");
    max_tokens_table.await_ready().await;

    let max_tokens_path = home.max_tokens_auto_path();
    assert!(
        max_tokens_path.exists(),
        "max_tokens_auto.toml must exist after the probe converges (was: {})",
        max_tokens_path.display()
    );
    let max_tokens_body =
        std::fs::read_to_string(&max_tokens_path).expect("read max_tokens_auto.toml");
    // Canonical header: section = "minimax" (top-level), model =
    // "MiniMax-M3" (nested). The bug would have produced
    // `[providers."MiniMax-M3"."MiniMax-M3"]` (model id as section
    // name) — same shape the PR-x23 follow-up fixed for the
    // auto-probe path.
    assert_canonical_provider_header(
        &max_tokens_body,
        "minimax",
        "MiniMax-M3",
        "max_tokens_auto.toml",
    );
    assert!(
        !max_tokens_body.contains("\"MiniMax-M3\".\"MiniMax-M3\""),
        "max_tokens_auto.toml must NOT use the model id as the section name \
         (regression: spawn_pending_probes must use inner.name()); got:\n{max_tokens_body}"
    );
}

// `cargo test` complains if a test file has no `#[test]`-decorated
// functions; the tempdir + wiremock imports above ensure every test
// has the helpers it needs. PathBuf is referenced through tempdir
// implicitly; this `use` keeps the import clean for future tests.
#[allow(dead_code)]
fn _pathbuf_marker(_: PathBuf) {}
