//! End-to-end regression tests for the dispatch gate in
//! `RunContext::dispatch_to_provider` that clamps a
//! `Request.temperature` to the nearest value in the
//! `TemperatureTable` supported set.
//!
//! These tests pin the three documented branches:
//!
//! 1. Requested temperature already in the supported set →
//!    passes through verbatim, no clamp, no warning.
//! 2. Requested temperature outside the supported set → snaps
//!    to the nearest value (tiebreak: first appearance in the
//!    set, the same convention `nearest_in_set` documents).
//! 3. Supported set empty (no cached entry, or empty entry) →
//!    passes through verbatim, no clamp.
//!
//! The tests build a `RunContext` whose default provider is a
//! `RecordingProvider` that captures every `Request` it receives,
//! drive `call_with_retry_at_temp` (the helper the discovery
//! matrix uses per `(cell, temperature, replica)`), and assert
//! the captured `temperature` field reflects the gate's verdict.
//! The `RecordingProvider` is duplicated from
//! `src/phases/phase.rs::tests` (the same struct already exists
//! in the unit-test module) to avoid widening the public API of
//! `phases` for a test-only helper — duplication is cheap (≈20
//! lines) and the encapsulation stays intact.

use std::sync::Arc;

use async_trait::async_trait;
use moagan::config::{Config, ProviderConfig};
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::temperature_probe::{Entry, TemperatureTable, TemperatureTableFile};
use moagan::llm::{Provider, ProviderRegistry, Request, Response, Role};
use moagan::phases::RunContext;
use moagan::telemetry::Telemetry;
use tempfile::TempDir;

/// Provider that captures the most-recent `Request` it received.
/// Mirrors the `RecordingProvider` defined in
/// `src/phases/phase.rs::tests` (kept private to that module);
/// the duplication is intentional — promoting the test helper to
/// `pub` would leak an internal test artefact into the public
/// surface of `phases` for no real benefit. The `captured` slot
/// is shared via `Arc<parking_lot::Mutex<...>>` so the test body
/// can read what `send` recorded after the call returns.
struct RecordingProvider {
    captured: Arc<parking_lot::Mutex<Option<Request>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }
    fn model(&self) -> &str {
        "recording-model"
    }
    fn endpoint(&self) -> &str {
        "mock://recording"
    }
    async fn send(&self, req: &Request) -> Result<(u16, Response), moagan::error::Error> {
        *self.captured.lock() = Some(req.clone());
        Ok((
            200,
            Response {
                text: r#"{"ok":true}"#.into(),
                finish_reason: Some("end_turn".into()),
                truncated: false,
                usage: Default::default(),
            },
        ))
    }
}

/// Build a `TemperatureTable` from a hand-written TOML sidecar
/// carrying a single `(provider, model) → Entry` whose
/// `temperatures` is exactly the supplied set. Persistence is
/// disabled (`save = false`) so the test does not rewrite the
/// file on subsequent probes; the table's effective set is
/// exactly the supplied slice, irrespective of any on-disk
/// sidecar a previous run might have left behind.
fn temperature_table_for_test(provider: &str, model: &str, temps: &[f32]) -> Arc<TemperatureTable> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("temperatures_auto.toml");
    let mut file = TemperatureTableFile::new_empty();
    file.providers
        .entry(provider.to_owned())
        .or_default()
        .insert(
            model.to_owned(),
            Entry {
                temperatures: temps.to_vec(),
                detected_at: "2026-08-23T00:00:00Z".to_owned(),
                verified_at: "2026-08-23T00:00:00Z".to_owned(),
                auto: true,
                attempts: 1,
            },
        );
    file.save(&path).expect("save temperatures_auto.toml");
    let table = TemperatureTable::from_path(&path, false).expect("from_path");
    Arc::new(table)
}

/// Build a `RunContext` whose default provider is a
/// freshly-constructed `RecordingProvider`. The returned
/// `Arc<parking_lot::Mutex<Option<Request>>>` is the slot the
/// provider writes into on every `send`, so each test can read
/// the request body the dispatch gate actually transmitted. The
/// per-provider `temperature` stays `None` so the per-role
/// default (`Sketch = 1.0`) does not contaminate the assertion;
/// `call_with_retry_at_temp` stamps the explicit `temperature`
/// parameter straight onto `Request.temperature`, so the gate
/// sees the test's chosen value verbatim.
fn build_context(
    table: Option<Arc<TemperatureTable>>,
) -> (
    TempDir,
    RunContext,
    Arc<parking_lot::Mutex<Option<Request>>>,
) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
    home.ensure().expect("MoaganHome::ensure");
    let run_id = RunId::new();
    let telemetry = Telemetry::open(
        run_id,
        &home.run_dir(run_id),
        moagan::redact::RedactPolicy::default(),
        None,
    )
    .expect("Telemetry::open");

    let captured = Arc::new(parking_lot::Mutex::new(None));
    let provider: Arc<RecordingProvider> = Arc::new(RecordingProvider {
        captured: Arc::clone(&captured),
    });
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let mut registry = ProviderRegistry::default();
    registry.insert("recording".into(), provider_dyn);

    let mut cfg = Config::default();
    cfg.providers.insert(
        "recording".to_owned(),
        ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            plan: None,
        },
    );
    let ctx = RunContext::new_with_config(
        run_id,
        home,
        Arc::new(registry),
        "recording".into(),
        "recording-model".into(),
        Parallelism::new(1),
        telemetry,
        String::new(),
        "standard".into(),
        Arc::new(cfg),
    )
    .with_temperature_table_opt(table);

    (temp, ctx, captured)
}

/// Gate branch #1: requested temperature already in the
/// supported set. The dispatch path must leave the request
/// untouched — no clamp, no warning, the provider sees the
/// value verbatim. Without this branch the gate would silently
/// rewrite every legal request, which is the failure mode the
/// audit surfaced and the reason the gate exists in the first
/// place: the operator's profile should pass through when the
/// upstream actually accepts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clamp_passes_through_when_temperature_in_set() {
    let table = temperature_table_for_test("recording", "recording-model", &[0.0, 0.5, 1.0]);
    let (_temp, ctx, captured) = build_context(Some(table));
    let _ = ctx
        .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 0.5)
        .await
        .expect("call should succeed");
    let recorded = captured
        .lock()
        .clone()
        .expect("provider captured the request");
    assert_eq!(
        recorded.temperature,
        Some(0.5),
        "value already in the supported set must reach the provider verbatim"
    );
}

/// Gate branch #2: requested temperature outside the supported
/// set. The dispatch path snaps the requested value to the
/// nearest cached temperature (tiebreak: first appearance in
/// the sorted supported set). For `0.7` against `[0.0, 0.5,
/// 1.0]` both `0.5` (distance 0.2) and `1.0` (distance 0.3) are
/// candidates, so the snap lands on `0.5`. The captured
/// request reflects the snapped value — the audit pin for the
/// "no LLM call escapes the supported set" contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clamp_runs_when_temperature_out_of_set() {
    let table = temperature_table_for_test("recording", "recording-model", &[0.0, 0.5, 1.0]);
    let (_temp, ctx, captured) = build_context(Some(table));
    let _ = ctx
        .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 0.7)
        .await
        .expect("call should succeed");
    let recorded = captured
        .lock()
        .clone()
        .expect("provider captured the request");
    assert_eq!(
        recorded.temperature,
        Some(0.5),
        "out-of-range 0.7 must snap to the nearest supported value (0.5)"
    );
}

/// Gate branch #3: supported set is empty (the cached entry has
/// no temperatures — e.g. the probe returned `Vec::new()` on a
/// uniformly-rejecting upstream). The gate must short-circuit
/// without touching the request, so the operator's literal
/// `temperature = 0.7` reaches the upstream verbatim. This is
/// the production contract for the "probe gave no signal" case
/// documented in the plan's risks section: a probe that
/// uniformly rejects is indistinguishable from a probe that
/// did not run, so the gate stays silent and the runtime
/// falls back to the legacy "send whatever was requested"
/// behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clamp_passes_through_when_table_empty() {
    let table = temperature_table_for_test("recording", "recording-model", &[]);
    let (_temp, ctx, captured) = build_context(Some(table));
    let _ = ctx
        .call_with_retry_at_temp(Role::Sketch, String::new(), String::new(), 0, 0.7)
        .await
        .expect("call should succeed");
    let recorded = captured
        .lock()
        .clone()
        .expect("provider captured the request");
    assert_eq!(
        recorded.temperature,
        Some(0.7),
        "empty supported set must leave the request untouched"
    );
}
