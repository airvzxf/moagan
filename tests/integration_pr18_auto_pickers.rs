//! PR-18 integration test: verify that
//! `DiscoveryCoordinator::run_with_ctx_and_target` auto-invokes
//! `run_with_pickers` at the START of the discovery loop when
//! `Config::discovery.auto_pickers` is `true`.
//!
//! Spec reference: docs/v0.5-roadmap.md PR-18 (D.13.18).
//!
//! The contract:
//!
//! 1. When `auto_pickers = true` AND `persona_enabled = true` AND
//!    `angle_enabled = true`, the coordinator's
//!    `run_with_ctx_and_target` issues:
//!    - one `Role::PersonaPicker` LLM call,
//!    - one `Role::AnglePicker` LLM call,
//!    - N `Role::Sketch` LLM calls (one per matrix cell).
//! 2. The `calls.jsonl.gz` sidecar (gzip JSONL) records every call
//!    in `started_unix` order. The PR-18 promise — "`persona_picker`
//!    and `angle_picker` BEFORE `matrix_generator`" — is the same
//!    as "`persona_picker` and `angle_picker` BEFORE the first
//!    `Role::Sketch` (matrix generation) row".
//!
//! The test pins both promises with concrete assertions on the
//! audit sidecar. The matrix is intentionally small
//! (`default_for(8)` → 8 cells × 1 sketch_per_cell = 8 sketch
//! calls) so the test stays fast while still exercising every
//! `(cell, sketch_index)` fan-out path.

// The env mutex is intentionally held across `await` points so
// two test threads cannot both flip `MOAGAN_HOME` mid-flight.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::cancel::Cancel;
use moagan::config::{Config, DiscoveryWiringConfig};
use moagan::discovery::DiscoveryCoordinator;
use moagan::domain::Brief;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::{MockProvider, MockResponse, ProviderRegistry};
use moagan::phases::RunContext;
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

/// Process-wide mutex that serialises every test which mutates
/// the `MOAGAN_HOME` env var.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Persona picker response. The helper appends this string to the
/// queue and the canonical
/// `Role::PersonaPicker` call returns it; the coordinator then
/// records a `persona_picker` row in `calls.jsonl.gz`.
fn persona_picker_payload() -> &'static str {
    r#"{
      "selected": "skeptic",
      "rationale": "audit-first mindset catches corner cases",
      "schema_version": "persona_picker.v1"
    }"#
}

/// Angle picker response. Returns a unique angle id and the
/// canonical angle:prefixed-strategy so the legacy mutation in
/// `persona_angle::pick_angle` is exercised end-to-end.
fn angle_picker_payload() -> &'static str {
    r#"{
      "problem": "Design a multi-tenant SaaS backend",
      "existing_angles": ["deployment-model / serverless", "storage / SQL"],
      "selected": "oauth2_pkce",
      "rationale": "delegates to a trusted IdP",
      "schema_version": "angle_picker.v1"
    }"#
}

/// Sketch payload the mock surfaces for every `Role::Sketch`
/// call. The 35-char thesis clears the matrix phase's 30-char
/// minimum-thesis gate.
fn sketch_payload(id: &str) -> String {
    format!(
        r#"{{
          "id": "{id}",
          "thesis": "Use Rust and SQLite for a single binary backend with strong typing.",
          "key_decisions": ["single binary", "embedded sqlite"],
          "architecture_outline": "The CLI binary owns the database, the cache, and the agent registry.",
          "assumptions": ["users are comfortable with one process per run"],
          "strengths": ["simple deployment", "easy to test"],
          "weaknesses": ["no horizontal scaling"],
          "hard_constraint_check": {{"single_binary": true}},
          "expected_validation": "Build a 1k-line Rust crate that compiles in <2s.",
          "angle": "minimalist"
        }}"#
    )
}

/// Build a `MockProvider` queue sized for the 8-cell matrix: 1
/// persona-picker response + 1 angle-picker response + 8 sketch
/// responses = 10 total. The first two are consumed by the
/// coordinator's auto-invoke (before the matrix fan-out); the
/// remaining 8 are consumed by the matrix loop, one per
/// `(cell, sketch_index)` pair.
fn build_auto_pickers_mock() -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(persona_picker_payload()));
    p.push(MockResponse::plain(angle_picker_payload()));
    for n in 0..8 {
        p.push(MockResponse::plain(sketch_payload(&format!("sk_{n:04}"))));
    }
    p.set_cycle(true);
    Arc::new(p)
}

/// Build a `MockProvider` queue sized for the 8-cell matrix when
/// `auto_pickers = false`. Only the 8 sketch responses are needed
/// (the pickers never fire). `set_cycle(true)` so the test stays
/// deterministic without counting every retry — sketch parsing
/// always succeeds with the canonical payloads.
fn build_matrix_only_mock() -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    for n in 0..8 {
        p.push(MockResponse::plain(sketch_payload(&format!("sk_{n:04}"))));
    }
    p.set_cycle(true);
    Arc::new(p)
}

/// Build a `RunContext` wired to the supplied mock provider with
/// a config that has `auto_pickers = true` (the default) plus
/// both catalogue flags enabled. The custom `DiscoveryWiringConfig`
/// is the surface this PR introduces — without it the catalogue
/// roles would never be invoked.
fn build_ctx_with_auto_pickers(
    home: Arc<MoaganHome>,
    run_id: RunId,
    run_dir: &moagan::fs_layout::RunDir<'_>,
    mock: Arc<MockProvider>,
) -> RunContext {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = mock.clone();
    registry.insert("mock".into(), arc);
    let telemetry =
        Telemetry::open(run_id, run_dir, RedactPolicy::default(), None).expect("open telemetry");
    let cfg = Arc::new(Config {
        discovery: DiscoveryWiringConfig {
            persona_enabled: true,
            angle_enabled: true,
            angle_clusters_min: 2,
            ..DiscoveryWiringConfig::default()
        },
        // F1 (Track G.2): pre-populate `matrix_spec` with the
        // legacy 4×2 layout so the coordinator's matrix builder
        // has a non-empty shape to fan out against.
        discovery_matrix: moagan::config::DiscoveryMatrixConfig {
            matrix_spec: vec![
                "a=x,y".to_string(),
                "b=x,y".to_string(),
                "c=x,y".to_string(),
                "d=x,y".to_string(),
            ],
            ..moagan::config::DiscoveryMatrixConfig::default()
        },
        ..Config::default()
    });
    RunContext::new_with_config(
        run_id,
        home,
        Arc::new(registry),
        "mock".to_owned(),
        "mock-model".to_owned(),
        Parallelism::new(1),
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".to_owned(),
        cfg,
    )
}

/// Seed a brief under the canonical `<run_dir>/brief.json` path
/// so the coordinator's matrix phase sees a non-empty brief.
fn seed_brief(run_dir: &moagan::fs_layout::RunDir<'_>) {
    let brief = serde_json::json!({
        "problem": "Design a multi-tenant SaaS backend",
        "objectives": ["Auth", "Storage"],
        "constraints": ["Rust single binary"],
        "non_goals": [],
        "open_questions": [],
        "raw_prompt": "Design a multi-tenant SaaS backend"
    });
    std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();
}

/// Read `telemetry/calls.jsonl.gz` (gzip JSONL, one event per
/// line) and decode every line as a generic `Value`. The calls
/// file is the canonical source of truth for the order of LLM
/// calls; the audit `verify` CLI consumes the same file.
fn read_calls_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    let metadata = std::fs::metadata(path).expect("stat calls.jsonl.gz");
    if metadata.len() == 0 {
        return Vec::new();
    }
    let bytes = std::fs::read(path).expect("read calls.jsonl.gz");
    let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut raw = Vec::new();
    use std::io::Read;
    decoder.read_to_end(&mut raw).expect("gunzip calls.jsonl");
    raw.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("calls.jsonl json"))
        .collect()
}

/// Find the index of the first row in `entries` whose `role`
/// equals `target_role`. Returns `None` if no such row exists.
fn first_role_index(entries: &[serde_json::Value], target_role: &str) -> Option<usize> {
    entries
        .iter()
        .position(|e| e.get("role").and_then(|v| v.as_str()) == Some(target_role))
}

/// PR-18 contract: when `auto_pickers = true` AND the catalogue
/// flags are enabled, the coordinator auto-invokes `pick_persona`
/// and `pick_angle` BEFORE the matrix fan-out. The audit sidecar
/// `calls.jsonl.gz` records every LLM call; the PR-18 promise
/// reduces to "`persona_picker` and `angle_picker` rows precede
/// the first `Role::Sketch` (matrix-generation) row".
///
/// The mock provider surfaces 10 distinct responses: 1 persona
/// picker + 1 angle picker + 8 sketches. With the auto-invoke
/// logic in place, every one of those 10 calls lands in
/// `calls.jsonl.gz` in the order they were issued, and the two
/// picker rows are the first two entries — ahead of every
/// `Role::Sketch` row.
#[tokio::test]
async fn pr18_auto_pickers_emit_picker_rows_before_matrix() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();

    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    seed_brief(&run_dir);

    let mock = build_auto_pickers_mock();
    let ctx = Arc::new(build_ctx_with_auto_pickers(
        home.clone(),
        run_id,
        &run_dir,
        mock.clone(),
    ));

    let coordinator = DiscoveryCoordinator::new(
        (*home).clone(),
        run_id,
        Cancel::new(),
        Brief::default(),
        "deployment-model:serverless".to_owned(),
        // Standard mode: soft target = midpoint(5..10) = 7, which
        // clears the persona picker's `target > 4` gate (Fast mode
        // targets 4 and would skip the picker). The matrix still
        // fans out to 8 cells × 1 per cell = 8 sketches.
        moagan::cli::Mode::Standard,
    );
    let outcome = coordinator
        .run_with_ctx(ctx.clone())
        .await
        .expect("auto-pickers run should succeed");

    assert_eq!(
        outcome.sketches_completed, 8,
        "matrix cardinality (8 cells × 1 per cell) must be reached end-to-end"
    );
    ctx.telemetry.flush().expect("telemetry flush");

    let calls_path = ctx.telemetry.calls_path().to_path_buf();
    let entries = read_calls_jsonl(&calls_path);

    // The mock recorded every LLM call the coordinator issued:
    // 1 persona picker + 1 angle picker + 8 sketch calls = 10.
    assert_eq!(
        entries.len(),
        10,
        "calls.jsonl.gz must hold one row per LLM call (1 persona + 1 angle + 8 sketch)"
    );
    assert_eq!(
        mock.calls().len(),
        10,
        "mock provider must observe 10 LLM calls (1 persona + 1 angle + 8 sketch)"
    );

    let persona_idx = first_role_index(&entries, "persona_picker")
        .expect("calls.jsonl.gz must contain a persona_picker row");
    let angle_idx = first_role_index(&entries, "angle_picker")
        .expect("calls.jsonl.gz must contain an angle_picker row");
    let sketch_idx = first_role_index(&entries, "sketch")
        .expect("calls.jsonl.gz must contain at least one sketch (matrix generation) row");

    assert!(
        persona_idx < sketch_idx,
        "persona_picker row (index {persona_idx}) must precede the first sketch (matrix) \
         row (index {sketch_idx}) — auto-invoke must happen BEFORE the matrix fan-out"
    );
    assert!(
        angle_idx < sketch_idx,
        "angle_picker row (index {angle_idx}) must precede the first sketch (matrix) \
         row (index {sketch_idx}) — auto-invoke must happen BEFORE the matrix fan-out"
    );
    // Persona and angle are issued sequentially at the start of
    // discovery; both must land before any matrix call. The
    // helper ordering — persona first, angle second — matches
    // the coordinator's `run_with_ctx_and_target` body.
    assert!(
        persona_idx < angle_idx,
        "persona_picker must fire before angle_picker at the start of discovery"
    );
}

/// PR-18 opt-out: when `auto_pickers = false`, the coordinator
/// must NOT issue any `persona_picker` or `angle_picker` calls
/// even when the catalogue flags are enabled. The audit sidecar
/// records exactly N matrix-generation rows (no picker rows).
#[tokio::test]
async fn pr18_auto_pickers_disabled_skips_picker_rows() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();

    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    seed_brief(&run_dir);

    let mock = build_matrix_only_mock();
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = mock.clone();
    registry.insert("mock".into(), arc);
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("open telemetry");
    let cfg = Arc::new(Config {
        discovery: DiscoveryWiringConfig {
            // Catalogue flags stay on so the *individual*
            // short-circuits are not the reason the picker rows
            // are missing — the master `auto_pickers = false`
            // switch must be the gating factor.
            persona_enabled: true,
            angle_enabled: true,
            auto_pickers: false,
            ..DiscoveryWiringConfig::default()
        },
        // F1 (Track G.2): pre-populate `matrix_spec` with the
        // legacy 4×2 layout so the coordinator's matrix builder
        // has a non-empty shape to fan out against.
        discovery_matrix: moagan::config::DiscoveryMatrixConfig {
            matrix_spec: vec![
                "a=x,y".to_string(),
                "b=x,y".to_string(),
                "c=x,y".to_string(),
                "d=x,y".to_string(),
            ],
            ..moagan::config::DiscoveryMatrixConfig::default()
        },
        ..Config::default()
    });
    let ctx = Arc::new(RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::new(registry),
        "mock".to_owned(),
        "mock-model".to_owned(),
        Parallelism::new(1),
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".to_owned(),
        cfg,
    ));

    let coordinator = DiscoveryCoordinator::new(
        (*home).clone(),
        run_id,
        Cancel::new(),
        Brief::default(),
        "deployment-model:serverless".to_owned(),
        // Standard mode: soft target = midpoint(5..10) = 7, which
        // clears the persona picker's `target > 4` gate (Fast mode
        // targets 4 and would skip the picker). The matrix still
        // fans out to 8 cells × 1 per cell = 8 sketches.
        moagan::cli::Mode::Standard,
    );
    let outcome = coordinator
        .run_with_ctx(ctx.clone())
        .await
        .expect("auto-pickers run should succeed");

    assert_eq!(
        outcome.sketches_completed, 8,
        "matrix cardinality (8 cells × 1 per cell) must be reached end-to-end"
    );
    ctx.telemetry.flush().expect("telemetry flush");

    let calls_path = ctx.telemetry.calls_path().to_path_buf();
    let entries = read_calls_jsonl(&calls_path);

    // 8 sketch rows + NO persona / angle rows.
    assert_eq!(
        entries.len(),
        8,
        "calls.jsonl.gz must hold only the 8 matrix-generation rows when auto_pickers=false"
    );
    assert!(
        first_role_index(&entries, "persona_picker").is_none(),
        "auto_pickers=false must suppress the persona_picker row"
    );
    assert!(
        first_role_index(&entries, "angle_picker").is_none(),
        "auto_pickers=false must suppress the angle_picker row"
    );
}
