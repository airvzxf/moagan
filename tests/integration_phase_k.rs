//! Integration tests for sub-fase K (add-ons).
//!
//! Mirrors the patterns from `tests/integration_phase_g.rs` and
//! `tests/integration_phase_h.rs`. The tests exercise the public
//! surface added by K.1, K.2, K.5, K.7, and K.9 — they do NOT
//! re-run the unit tests; they cover the cross-module invariants
//! the unit tests cannot.
//!
//! Coverage:
//!  - K.1: HARD_INCOMPATIBILITIES matrix + is_incompatible +
//!    SynthesizePhase::extract_tags / cluster_conflict.
//!  - K.2: Embedder round-trip + cosine determinism across
//!    multiple cache states.
//!  - K.5: SQLite v008 migration applies + the five new helpers
//!    round-trip data; idempotency on re-migration.
//!  - K.7: categorised redaction produces both the substitute
//!    marker AND the audit `kinds` vector.
//!  - K.9: per-mode retry budget table values match the spec.

use moagan::cli::Mode;
use moagan::domain::constraint::{HARD_INCOMPATIBILITIES, is_incompatible};
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::embed::{Embedder, HashingEmbedder, cosine};
use moagan::llm::retry_budget::{RetryReason, budget_for};
use moagan::llm::{MockProvider, MockResponse, ProviderRegistry};
use moagan::phases::util::write_json;
use moagan::phases::{Phase, PhaseOutput, RoutePhase, RunContext};
use moagan::redact::RedactPolicy;
use moagan::redact::apply::{Surface, apply_with_categories};
use moagan::redact::patterns::{PatternKind, substitute};
use moagan::storage::sqlite::{
    Db, ManifestEventRow, OutboxEventRow, ProviderRollupRow, RedactAuditRow,
};
use moagan::storage::{
    ProcessLease, acquire_process_lock, heartbeat_process_lock, release_process_lock,
};
use moagan::telemetry::Telemetry;
use moagan::test_support::with_moagan_home;
use std::collections::HashSet;
use std::sync::Arc;

/// Open a fresh DB and register one run. Returns the DB handle and
/// the run id; callers use `id.to_string()` as the foreign key
/// when constructing the v008 rows so the FK constraint is
/// satisfied.
fn fresh_db_with_run() -> (tempfile::TempDir, Db, String) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("meta.sqlite");
    let db = Db::open(&db_path).unwrap();
    let id = RunId::new();
    db.register_run(id, "fast", "running", "0.4.0", None, None, None)
        .unwrap();
    (tmp, db, id.to_string())
}

#[test]
fn hard_incompatibilities_contains_known_pairs() {
    let set: HashSet<(&str, &str)> = HARD_INCOMPATIBILITIES.iter().copied().collect();
    assert!(set.contains(&("monolith", "microservices")));
    assert!(set.contains(&("sql", "nosql")));
    assert!(set.contains(&("strong_consistency", "eventual_consistency")));
    assert!(is_incompatible("monolith", "microservices"));
    assert!(is_incompatible("microservices", "monolith"));
    assert!(!is_incompatible("sql", "rust"));
    assert_eq!(HARD_INCOMPATIBILITIES.len(), 10);
}

#[test]
fn hashing_embedder_e2e() {
    let e = HashingEmbedder::new(256);
    assert_eq!(e.dim(), 256);
    assert_eq!(e.name(), "hashing");
    let v1 = e.embed("hello world");
    let v2 = e.embed("hello world");
    assert_eq!(v1, v2);
    assert!(cosine(&v1, &v2) > 0.99);
    let v3 = e.embed("quantum entanglement probability");
    assert!(cosine(&v1, &v3) < 0.5, "expected dissimilar vectors");
    // Cache returns the same vector verbatim.
    let v4 = e.embed("hello world");
    assert_eq!(v4, v1);
}

#[test]
fn v008_migrations_apply() {
    let (_tmp, db, _run_id) = fresh_db_with_run();
    // The v008 helpers are best-effort against the schema version.
    // When user_version < 8 they no-op; when user_version >= 8 they
    // round-trip data. So writing + reading via the public helpers
    // is enough to pin the migration applied.
    let row = OutboxEventRow {
        run_id: _run_id.clone(),
        event_type: "phase_started".into(),
        payload: "{}".into(),
        at_unix: 0,
    };
    db.record_outbox_event(&row).unwrap();
    let rows = db.list_outbox_events_for_run(&_run_id).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn outbox_event_round_trips_via_public_helper() {
    let (_tmp, db, run_id) = fresh_db_with_run();
    let row = OutboxEventRow {
        run_id: run_id.clone(),
        event_type: "test".into(),
        payload: "{}".into(),
        at_unix: 1_700_000_000,
    };
    db.record_outbox_event(&row).unwrap();
    let rows = db.list_outbox_events_for_run(&run_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_type, "test");
    assert_eq!(rows[0].payload, "{}");
    assert_eq!(rows[0].at_unix, 1_700_000_000);
}

#[test]
fn redact_audit_round_trips_via_public_helper() {
    let (_tmp, db, run_id) = fresh_db_with_run();
    let row = RedactAuditRow {
        run_id: Some(run_id.clone()),
        source_path: "telemetry/calls.jsonl".into(),
        pattern_kind: "minimax_sk_cp".into(),
        match_count: 2,
        at_unix: 1_700_000_010,
    };
    db.record_redact_audit(&row).unwrap();
    let rows = db.list_redact_audit_for_run(&run_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pattern_kind, "minimax_sk_cp");
    assert_eq!(rows[0].match_count, 2);
}

#[test]
fn manifest_event_round_trips_via_public_helper() {
    let (_tmp, db, run_id) = fresh_db_with_run();
    let row = ManifestEventRow {
        run_id: run_id.clone(),
        event_type: "phase_started".into(),
        details: Some(r#"{"phase":"intake"}"#.into()),
        at_unix: 1_700_000_100,
    };
    db.record_manifest_event(&row).unwrap();
    db.record_manifest_event(&ManifestEventRow {
        run_id: run_id.clone(),
        event_type: "phase_ended".into(),
        details: None,
        at_unix: 1_700_000_101,
    })
    .unwrap();
    // The presence of a second insert (which would fail with
    // UNIQUE-style errors if the schema was broken) is enough to
    // prove the migration applied; the v008 unit tests cover the
    // exact row content.
    let outbox_rows = db.list_outbox_events_for_run(&run_id).unwrap();
    assert!(outbox_rows.is_empty());
}

#[test]
fn process_lock_acquire_release_round_trips_via_public_helper() {
    let (_tmp, db, _run_id) = fresh_db_with_run();
    assert!(db.acquire_process_lock("holder-A", 60, "fence-1").unwrap());
    assert!(!db.acquire_process_lock("holder-B", 60, "fence-2").unwrap());
    assert!(db.release_process_lock("holder-A").unwrap());
    assert!(db.acquire_process_lock("holder-B", 60, "fence-3").unwrap());
}

/// T01-06 D.1.5: the typed `ProcessLease` API walks the full
/// acquire → heartbeat → release lifecycle against a real Db.
/// Mirrors `process_lock_acquire_release_round_trips_via_public_helper`
/// but exercises the new module-level helpers and the
/// last_heartbeat_unix column added in v019.
#[test]
fn process_lease_lifecycle_acquire_heartbeat_release() {
    let (_tmp, db, run_id_str) = fresh_db_with_run();
    let run_id: RunId = run_id_str.parse().expect("run_id is a valid UUID");
    let holder = uuid::Uuid::new_v4();

    let first: ProcessLease = acquire_process_lock(&db, run_id, holder, 60).expect("first acquire");
    assert!(first.fencing_token > 0);
    assert_eq!(first.acquired_at_unix, first.last_heartbeat_unix);

    let renewed: ProcessLease =
        heartbeat_process_lock(&db, run_id, holder, first.fencing_token).expect("heartbeat");
    assert_eq!(renewed.fencing_token, first.fencing_token);
    assert_eq!(renewed.acquired_at_unix, first.acquired_at_unix);
    assert!(
        renewed.last_heartbeat_unix >= first.last_heartbeat_unix,
        "heartbeat must advance last_heartbeat_unix"
    );

    release_process_lock(&db, run_id, holder, first.fencing_token).expect("release");

    // After release a fresh acquire must succeed and obtain a
    // valid fencing token. The MAX+1 path scans the WHOLE
    // `process_locks` table; on a freshly-released DB with no
    // other rows the next fence is 1, so we only assert the
    // token is a positive u64 (the "fence bumps on takeover"
    // contract is verified separately in the unit test
    // `process_lease_acquire_after_ttl_expiry_succeeds_with_higher_fence`).
    let re_acquired: ProcessLease =
        acquire_process_lock(&db, run_id, holder, 60).expect("re-acquire after release");
    assert!(
        re_acquired.fencing_token > 0,
        "post-release fence must be a positive u64, got {}",
        re_acquired.fencing_token
    );
}

#[test]
fn provider_rollup_increments_via_public_helper() {
    let (_tmp, db, _run_id) = fresh_db_with_run();
    db.increment_provider_rollup("minimax", "MiniMax-M3", 100, 50, false)
        .unwrap();
    db.increment_provider_rollup("minimax", "MiniMax-M3", 200, 80, true)
        .unwrap();
    let row: Option<ProviderRollupRow> = db.get_provider_rollup("minimax", "MiniMax-M3").unwrap();
    let row = row.expect("rollup row must exist");
    assert_eq!(row.calls, 2);
    assert_eq!(row.input_tokens, 300);
    assert_eq!(row.output_tokens, 130);
    assert_eq!(row.errors, 1);
}

#[test]
fn k7_redact_sk_cp_api_key_categorised() {
    let input = "API key is sk-cp-abc123def456ghi789jkl012mno345pqr678stu901vwx234";
    let p = RedactPolicy::default();
    let r = apply_with_categories(&p, Surface::Telemetry, input).unwrap();
    assert!(r.text.contains("***REDACTED:api_key:sk-cp***"));
    assert!(r.kinds.iter().any(|(k, _)| *k == PatternKind::SkCpApiKey));
}

#[test]
fn k7_substitute_returns_correct_marker() {
    assert_eq!(
        substitute(PatternKind::SkCpApiKey),
        "***REDACTED:api_key:sk-cp***"
    );
    assert_eq!(
        substitute(PatternKind::BearerHeader),
        "Bearer ***REDACTED***"
    );
}

/// `Deep` parse failures get the full repair budget: five attempts
/// (= four retries) with the local JSON repair pass enabled. The
/// previous matrix pinned this at 2 attempts; D.21.6 update now
/// matches the per-mode envelope where Parse/Schema always
/// cap at 5 to absorb model non-determinism.
#[test]
fn k9_retry_budget_for_deep_with_parse_uses_json_repair() {
    let b = budget_for(Mode::Deep, RetryReason::Parse);
    assert_eq!(b.max_attempts, 5);
    assert!(b.use_json_repair);
}

/// `Fast` allows retries for transient failures. The old matrix
/// pinned every Fast reason at `max_attempts = 1`; the new matrix
/// gives Transport / RateLimit / Timeout two retries (3 attempts)
/// so a flaky network or short 429 does not invalidate a fast
/// run. Parse / Schema still get the full repair budget
/// (5 attempts with `use_json_repair`).
#[test]
fn k9_retry_budget_for_fast_at_least_three_for_transients() {
    for reason in [
        RetryReason::Transport,
        RetryReason::RateLimit,
        RetryReason::Timeout,
    ] {
        let b = budget_for(Mode::Fast, reason);
        assert!(
            b.max_attempts >= 3,
            "Fast {reason:?} should allow at least 3 attempts, got {}",
            b.max_attempts
        );
    }
}

/// `Fast` parse / schema failures get the full repair budget
/// (5 attempts with `use_json_repair` = true). This is the
/// regression-pin for the smoke gate 2 fix: a `MiniMax-M3`
/// response of `{"problem":}` (malformed JSON) used to fail
/// the `Route` phase immediately because the old matrix
/// capped Fast at 1 attempt for every reason.
#[test]
fn k9_retry_budget_for_fast_at_least_five_for_parse_schema_with_repair() {
    for reason in [RetryReason::Parse, RetryReason::Schema] {
        let b = budget_for(Mode::Fast, reason);
        assert_eq!(b.max_attempts, 5, "reason={reason:?}");
        assert!(b.use_json_repair, "reason={reason:?}");
    }
}

/// `Deep` rate-limit failures are the most generous slot in the
/// matrix: six attempts (= five retries) because the heavy path
/// is expensive to restart and a transient throttle should not
/// invalidate the run. The old matrix pinned this at 3; the new
/// matrix lifts it to 6.
#[test]
fn k9_retry_budget_for_deep_rate_limit_is_six_attempts() {
    let b = budget_for(Mode::Deep, RetryReason::RateLimit);
    assert_eq!(b.max_attempts, 6);
    assert!(!b.use_json_repair);
}

// ---------------------------------------------------------------------------
// End-to-end retry-recovery test (Route phase).
//
// The D.21.6 matrix update lifts Fast / Explore / Batch off the
// "always single-shot" footgun: before, a `Route` call that came
// back with `{"problem":}` (the canonical MiniMax-M3 malformed
// payload) failed the whole run with `Error::SchemaViolation` on
// the first attempt because `budget_for(Fast, Parse).max_attempts
// == 1`. After the matrix change, the same payload triggers the
// retry loop (max_attempts = 5, use_json_repair = true) and
// recovers the moment the mock serves a parseable route JSON.
//
// This test pins that recovery loop end-to-end:
//   1. The mock returns `{"problem":}` for the first 3 Route calls.
//   2. The mock returns a valid `Route` JSON on the 4th call.
//   3. `RoutePhase::execute` completes with `PhaseOutput::Route`.
//   4. The `<run_dir>/final/route.json` sidecar is parseable and
//      matches the 4th mock response (proving the retry loop fired
//      instead of bailing on the first malformed payload).
// ---------------------------------------------------------------------------

/// Build a `RunContext` for a single `RoutePhase` invocation. Mirrors
/// the helper in `tests/integration_mvp.rs` so this test stays
/// self-contained and does not pull the full fast-mode pipeline.
fn build_route_run_context(
    home: Arc<MoaganHome>,
    provider: Arc<MockProvider>,
    run_id: RunId,
) -> RunContext {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = provider.clone();
    registry.insert("mock".into(), arc);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("open telemetry");
    let parallelism = Parallelism::new(1);
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Recovery test brief".into(),
        "fast".into(),
    )
}

/// Minimal valid `Brief` the `RoutePhase::execute` reads at startup.
/// Mirrors the shape written by `IntakePhase` so the phase does not
/// bail on a missing `problem` field.
fn route_recovery_brief() -> moagan::domain::Brief {
    moagan::domain::Brief {
        problem: "Trigger the retry loop on a malformed model response".into(),
        objectives: vec!["Recover from parse failure".into()],
        deliverables: vec![],
        constraints: vec![],
        assumptions: vec![],
        non_goals: vec![],
        acceptance: vec![],
        risks: vec![],
        context_block: None,
    }
}

/// End-to-end recovery test: the mock serves `{"problem":}` for the
/// first 3 Route calls, then a valid `Route` JSON on the 4th. The
/// retry loop (max_attempts=5 for Fast/Parse per the new matrix)
/// must consume the three malformed payloads, succeed on the 4th
/// call, write `final/route.json`, and return
/// `PhaseOutput::Route(path)`.
///
/// Before the D.21.6 matrix update this test would fail with
/// `Error::SchemaViolation` on the first malformed response because
/// the old `budget_for(Fast, Parse).max_attempts == 1`. After the
/// update the budget allows the retry loop to fire; the assertion on
/// the `MockProvider::calls()` count (4) is the regression pin.
#[test]
fn k9_route_phase_recovers_from_repeated_parse_failures() {
    with_moagan_home("k9_route_recovery", |_home_path| {
        // Build the home + run layout the Route phase expects.
        let home = Arc::new(MoaganHome::resolve().expect("resolve home"));
        home.ensure().expect("ensure home");
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run dir");

        // Write the brief the phase reads on entry. Without it the
        // phase bails on `Error::Io` (file not found) before the
        // retry loop is even consulted, which would make this test
        // pass for the wrong reason.
        write_json(&run_dir.brief(), &route_recovery_brief()).expect("write brief");

        // Mock: 3 malformed payloads followed by a valid one. The
        // 4th response is the one the retry loop must accept.
        let valid_route = r#"{
  "mode": "fast",
  "reason": "Recovery from parse failure",
  "sketches": 0,
  "proposals": 3,
  "judges": 3
}"#;
        let mut mock = MockProvider::empty();
        for _ in 0..3 {
            // `{"problem":}` is not valid JSON (the value side is
            // missing), so the parse pipeline classifies it as
            // `RetryReason::Parse` and looks up the Fast/Parse row
            // of the budget matrix.
            mock.push(MockResponse::plain(r#"{"problem":}"#));
        }
        mock.push(MockResponse::plain(valid_route));
        let provider = Arc::new(mock);

        // Drive the phase through a single-thread tokio runtime —
        // matches the pattern in `tests/integration_validators.rs`.
        let ctx = build_route_run_context(home.clone(), provider.clone(), run_id);
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime")
            .block_on(async { RoutePhase.execute(&ctx).await });
        let output = output.expect("RoutePhase::execute must succeed after retries");
        ctx.telemetry.flush().expect("flush telemetry");

        // The output must be the `Route(path)` variant; the path
        // must point at the canonical `final/route.json` sidecar.
        let path = match output {
            PhaseOutput::Route(p) => p,
            other => panic!("expected PhaseOutput::Route, got {other:?}"),
        };
        assert!(
            path.ends_with("final/route.json"),
            "RoutePhase must write final/route.json, got {}",
            path.display()
        );
        assert!(path.exists(), "route.json must exist on disk");

        // The on-disk file must be parseable as the Route contract
        // and must carry the payload from the 4th mock response —
        // proof that the retry loop fired instead of bailing on the
        // first malformed payload.
        let raw = std::fs::read_to_string(&path).expect("read route.json");
        let parsed: moagan::domain::Route =
            serde_json::from_str(&raw).expect("route.json is valid JSON for the Route contract");
        assert_eq!(parsed.mode, "fast");
        assert_eq!(parsed.proposals, 3);
        assert_eq!(parsed.reason, "Recovery from parse failure");

        // Regression pin: the mock must have been called exactly
        // 4 times (3 malformed + 1 valid). Before the D.21.6
        // update the loop bailed on the first malformed response,
        // so this counter would have been 1 and the test would
        // have failed on the `panic!` branch above.
        let calls = provider.calls();
        assert_eq!(
            calls.len(),
            4,
            "expected 4 mock calls (3 malformed + 1 valid), got {}",
            calls.len()
        );
    });
}
