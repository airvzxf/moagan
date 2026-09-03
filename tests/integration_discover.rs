//! Integration tests for `c3` (commit "feat(observability): emit
//! DiscoveryIteration events from sketch loop").
//!
//! The c3 invariant: `Event::DiscoveryIteration { n, total,
//! cell_dim, cell_facet, temperature, replica, sketch_index,
//! outcome }` is emitted to stdout for every sketch-loop iteration
//! in `moagan discover`. The pre-c3 stdout stream had no
//! per-iteration signal — operators watching the run could only
//! infer sketch progress from the periodic `iteration start`
//! trace lines.
//!
//! The test drives the real binary through `std::process::Command`
//! (mirroring `tests/integration_decisions.rs`) and asserts
//! against the on-disk `events.jsonl` file. The harness uses
//! `--matrix-spec` so the LLM-driven `discover_dimensions` phase
//! is skipped (the canned mock fixtures in
//! `tests/fixtures/mock_provider/` do not include a
//! `dimension_deriver` response, so without `--matrix-spec` the
//! run bails before the coordinator loop fires). With
//! `--matrix-spec` the matrix is built verbatim from the spec,
//! the coordinator spawns one task per `(cell, sketch_index,
//! replica, temperature)` tuple, and each task emits exactly
//! one `discovery_iteration` event.
//!
//! The wire-schema contract (the nine field names + the
//! `kind = "discovery_iteration"` discriminator) is also pinned
//! directly by a unit test in
//! `src/discovery/coordinator.rs::tests::discovery_iteration_event_serializes_with_kind_discriminator`,
//! so a schema rename trips the test even if the integration
//! smoke is skipped.
//!
//! Two assertions are exercised:
//!
//! 1. The mock discover path emits **at least one**
//!    `kind = "discovery_iteration"` event when the iteration
//!    loop runs end-to-end. With `--matrix-spec auth=oauth,api-key`
//!    × `--sketches-per-cell 10` (the F2 default; floor is 1 since v0.13.2) ×
//!    `--temperature-profile …replicas=1` the fan-out is
//!    `1 dim × 2 facets × 10 sketches × 1 temp × 1 replica = 20`
//!    iterations, so the expectation is `>= 1` (we do not pin a
//!    strict equal count because future F-changes might raise
//!    `sketches_per_cell` and the test would have to track the
//!    formula).
//!
//! 2. Every emitted `discovery_iteration` event carries the full
//!    nine-field schema (the same fields documented in
//!    `docs/events-v1.md` line 103). A future schema break —
//!    e.g. renaming `cell_dim` to `dimension` — surfaces here
//!    rather than silently in a downstream consumer.

use std::path::Path;
use std::process::Command;

use moagan::test_support::with_moagan_home;

/// Resolve the freshly built `moagan` binary. Mirrors
/// `tests/integration_decisions.rs::moagan_bin` so a plain
/// `cargo test --test integration_discover` invocation still
/// finds the binary.
fn moagan_bin() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_moagan")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("moagan")
        })
}

/// Read every JSONL line from `path` and parse it as a
/// `serde_json::Value`. Lines that don't start with `{` (the
/// discover CLI's operator-facing success banner — see
/// `src/cli/discover.rs:867` — emits a plain-text `moagan
/// discover …` line at the end of a successful run; this is a
/// pre-existing wart that's out of scope for c3) are skipped so
/// the NDJSON purity violation does NOT make the test panic on
/// a parse error. The full `DiscoverOutput.stderr_text` is
/// preserved for diagnostic surfacing on assertion failure.
fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| l.trim_start().starts_with('{'))
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .unwrap_or_else(|e| panic!("parse jsonl line {l:?}: {e}"))
        })
        .collect()
}

/// Count events with the given `kind` discriminator. Mirrors
/// the operator-facing `jq -c 'select(.kind == "<kind>")'
/// events.jsonl | wc -l`.
fn count_kind(events: &[serde_json::Value], kind: &str) -> usize {
    events
        .iter()
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some(kind))
        .count()
}

/// F2 (B4/T1): count `discovery_iteration` events attributed to
/// one `(section, model)` pair. The two fields were added by F2 so
/// a dashboard can split the sketch stream per provider; this
/// helper is the test-side equivalent of
/// `jq 'select(.kind == "discovery_iteration" and .section == "…"
/// and .model == "…")' | wc -l`.
fn count_iterations_for_pair(events: &[serde_json::Value], section: &str, model: &str) -> usize {
    events
        .iter()
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("discovery_iteration"))
        .filter(|v| v.get("section").and_then(|s| s.as_str()) == Some(section))
        .filter(|v| v.get("model").and_then(|m| m.as_str()) == Some(model))
        .count()
}

/// The wire-schema fields `Event::DiscoveryIteration` MUST
/// carry on every emit (per `docs/events-v1.md` line 103 and
/// the `Event::DiscoveryIteration` variant in
/// `src/telemetry/stdout_events.rs`). The set is duplicated here
/// on purpose — a future schema drop surfaces as a missing-field
/// assertion rather than silently shipping a wire-format change.
///
/// F2 (B4) added `section` + `model` so a dashboard consuming the
/// NDJSON stream can attribute each sketch to the provider pair
/// that produced it. The addition is additive, so `schema` stays
/// at `1` (see `docs/events-v1.md` §"Additive changes").
const ITER_REQUIRED_FIELDS: &[&str] = &[
    "schema",
    "ts",
    "n",
    "total",
    "section",
    "model",
    "cell_dim",
    "cell_facet",
    "temperature",
    "replica",
    "sketch_index",
    "outcome",
];

/// Drive a `moagan discover --provider mock:mock-model
/// --mock-dir tests/fixtures/mock_provider --matrix-spec …`
/// invocation with the smallest matrix that still exercises the
/// iteration loop end-to-end, capture stdout (events) and stderr
/// (tracing) to files under `work`, and return the parsed JSONL
/// events plus the binary's exit code and stderr text.
///
/// `--matrix-spec auth=oauth,api-key` declares exactly one
/// dimension with two facets (2 cells) and `--sketches-per-cell 10`
/// makes the per-cell fan-out = 10 (the F2 default; floor is 1 since v0.13.2). With
/// `temperatures=[0.5] × replicas=1` the total sketch attempts =
/// `1 × 2 × 10 × 1 × 1 = 20` iterations, so the lower-bound
/// assertion in the test (`>= 1`) is comfortably above the
/// fixture gap floor (the per-iteration loop fires at least once
/// before any post-matrix phase can fail).
fn drive_mock_discover(home: &Path, work: &Path) -> DiscoverOutput {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let stdout_path = work.join("events.jsonl");
    let stderr_path = work.join("log.jsonl");
    let output = Command::new(moagan_bin())
        .env("MOAGAN_HOME", home)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .arg("discover")
        .arg("--non-interactive")
        .arg("--prompt")
        // Spanish prompt — mirrors the prompt the existing
        // `integration_decisions.rs::drive_mock_run` uses so
        // anyone bisecting the two tests sees a familiar shape.
        .arg("Enumera los 7 colores del arcoiris en orden")
        .arg("--provider")
        .arg("mock:mock-model")
        .arg("--mock-dir")
        .arg(&mock_dir)
        // `--matrix-spec` skips the LLM-driven
        // `discover_dimensions` phase (no `dimension_deriver`
        // mock in the canned fixtures). The matrix is built
        // verbatim from the spec verbatim, so the iteration
        // loop fires immediately after the `intake` +
        // `clarify` phases.
        .arg("--matrix-spec")
        .arg("auth=oauth,api-key")
        .arg("--dimensions")
        .arg("1")
        .arg("--sketches-per-cell")
        .arg("10")
        // Single temperature + single replica → 2 × 10 = 20
        // iterations, all of which should clear the 30-char
        // thesis gate (`tests/fixtures/mock_provider/sketch/
        // 04-sketch-*.json` carry ~110-char theses).
        .arg("--temperature-profile")
        .arg("provider=mock;temperatures=0.5;replicas=1")
        .arg("--max-parallelism")
        .arg("1")
        .arg("--log-format")
        .arg("json")
        .arg("--event-format")
        .arg("jsonl")
        .stdout(std::fs::File::create(&stdout_path).expect("create events.jsonl"))
        .stderr(std::fs::File::create(&stderr_path).expect("create log.jsonl"))
        .output()
        .expect("spawn moagan discover");
    let exit_code = output.status.code();
    let events = read_jsonl(&stdout_path);
    let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    DiscoverOutput {
        events,
        exit_code,
        stderr_text,
    }
}

/// Captured output of a single `moagan discover` invocation.
struct DiscoverOutput {
    /// Parsed JSONL events from stdout.
    events: Vec<serde_json::Value>,
    /// Binary exit code. `Some(0)` on success; `Some(non-zero)`
    /// when the binary returned an error; `None` if the binary
    /// was killed by a signal (e.g. SIGPIPE from a downstream
    /// filter).
    #[allow(dead_code)]
    exit_code: Option<i32>,
    /// Raw stderr text (NDJSON expected under `--log-format json`).
    /// Held for debugging when an assertion fails so the
    /// operator sees the full failure trace, not just the
    /// parsed stdout events.
    #[allow(dead_code)]
    stderr_text: String,
}

/// `moagan discover` with `--matrix-spec` (skips the LLM
/// `discover_dimensions` phase) emits at least one
/// `kind = "discovery_iteration"` event for every sketch-loop
/// iteration. The test pins both halves of the contract:
///
/// * the event kind discriminator (`"discovery_iteration"`,
///   snake_case from the `#[serde(rename_all = "snake_case")]`
///   on the `Event` enum),
/// * the nine wire-schema fields documented in
///   `docs/events-v1.md` line 103.
///
/// The mock provider's canned sketches (`04-sketch-*.json`)
/// carry ~110-char theses, well above the 30-char
/// `accepted` gate in `src/discovery/coordinator.rs`, so every
/// iteration should fire an `outcome = "accepted"` event when
/// the upstream LLM response parses cleanly. The assertion is
/// `>= 1` rather than the exact count because the post-matrix
/// pipeline may abort on a downstream mock gap (`discover_facet`
/// / `discover_summary` need further fixtures); the c3 contract
/// is "events are emitted DURING the iteration loop", not
/// "the whole discover run succeeds end-to-end".
///
/// If the assertion fires, the failure message includes the
/// binary's exit code, the full stderr text, and the parsed
/// events vector so the regression source is obvious from a
/// single test log.
#[test]
fn discovery_iteration_event_emitted_per_sketch() {
    with_moagan_home("discovery_iteration_emit", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_mock_discover(home, work.path());
        let iter_count = count_kind(&out.events, "discovery_iteration");
        assert!(
            iter_count >= 1,
            "expected >=1 discovery_iteration events; got {iter_count}; \
             exit_code={:?}; stderr={}; events={}",
            out.exit_code,
            out.stderr_text,
            serde_json::to_string_pretty(&out.events).unwrap_or_default()
        );

        // Schema pin: every emitted event must carry all nine
        // wire fields + the `schema` discriminator. A regression
        // that drops a field (or renames it) trips this assertion
        // immediately rather than silently changing the wire
        // format for downstream consumers.
        let iter_events: Vec<&serde_json::Value> = out
            .events
            .iter()
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("discovery_iteration"))
            .collect();
        for (i, ev) in iter_events.iter().enumerate() {
            for field in ITER_REQUIRED_FIELDS {
                assert!(
                    ev.get(field).is_some(),
                    "discovery_iteration event #{i} missing required field {field:?}; got {ev}"
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tanda 04e D-1: --temperature-profile multi-provider wire-up
// ---------------------------------------------------------------------------

/// Drive a `moagan discover` invocation with two
/// `--temperature-profile` flags pinning different `(section,
/// model)` pairs. The mock provider serves the same canned
/// fixtures for every pair (the mock short-circuit re-uses one
/// `MockProvider` instance per `active_pair`), so the test can
/// assert on the per-pair fan-out count without needing real
/// upstream credentials.
///
/// Matrix shape:
/// - `--matrix-spec auth=oauth,api-key` -> 1 dim x 2 facets = 2 cells
/// - `--sketches-per-cell 2` -> 2 sketches per cell
/// - Profile A: `(mock, mock-model)` x `[0.5] x 1` = 1 iteration per cell
/// - Profile B: `(mock, other-model)` x `[0.7] x 1` = 1 iteration per cell
/// - The default `(mock, mock-model)` pair is suppressed because
///   an explicit entry exists for it
///
/// Total fan-out: `cells * per_cell * (1 + 1) = 2 x 2 x 2 = 8`.
/// The test asserts at least 8 `discovery_iteration` events.
fn drive_multi_provider_discover(home: &Path, work: &Path) -> DiscoverOutput {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let stdout_path = work.join("events.jsonl");
    let stderr_path = work.join("log.jsonl");
    let output = Command::new(moagan_bin())
        .env("MOAGAN_HOME", home)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .arg("discover")
        .arg("--non-interactive")
        .arg("--prompt")
        .arg("Enumera los 7 colores del arcoiris en orden")
        .arg("--provider")
        .arg("mock:mock-model")
        .arg("--mock-dir")
        .arg(&mock_dir)
        .arg("--matrix-spec")
        .arg("auth=oauth,api-key")
        .arg("--dimensions")
        .arg("1")
        .arg("--sketches-per-cell")
        .arg("2")
        // Tanda 04e D-1: two `--temperature-profile` flags with
        // the new `provider=<section>:<model>` form so the
        // coordinator fans out across two `(section, model)`
        // pairs.
        .arg("--temperature-profile")
        .arg("provider=mock:mock-model;temperatures=0.5;replicas=1")
        .arg("--temperature-profile")
        .arg("provider=mock:other-model;temperatures=0.7;replicas=1")
        .arg("--max-parallelism")
        .arg("1")
        .arg("--log-format")
        .arg("json")
        .arg("--event-format")
        .arg("jsonl")
        .stdout(std::fs::File::create(&stdout_path).expect("create events.jsonl"))
        .stderr(std::fs::File::create(&stderr_path).expect("create log.jsonl"))
        .output()
        .expect("spawn moagan discover multi-provider");
    let exit_code = output.status.code();
    let events = read_jsonl(&stdout_path);
    let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    DiscoverOutput {
        events,
        exit_code,
        stderr_text,
    }
}

/// Tanda 04e D-1: when two `--temperature-profile` flags pin two
/// distinct `(section, model)` pairs, the coordinator fans out
/// the matrix across BOTH pairs. The per-pair sketch count
/// sums so the total `discovery_iteration` event count matches
/// `cells * per_cell * Sigma profile.total()`.
///
/// Matrix: 2 cells x 2 sketches/cell = 4 sketches per provider.
/// Two providers -> 8 sketches total.
#[test]
fn discovery_iteration_event_count_matches_multi_provider_fanout() {
    with_moagan_home("discovery_multi_provider_iter_count", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_multi_provider_discover(home, work.path());
        let iter_count = count_kind(&out.events, "discovery_iteration");
        let expected = 8;
        assert!(
            iter_count >= expected,
            "expected >= {expected} discovery_iteration events (2 cells x 2 sketches x \
             2 providers); got {iter_count}; exit_code={:?}; stderr={}; events={}",
            out.exit_code,
            out.stderr_text,
            serde_json::to_string_pretty(&out.events).unwrap_or_default()
        );

        // F2 (T1): the aggregate count above passes even when one
        // pair produced every sketch and the other produced none —
        // exactly the regression `Event::DiscoveryIteration`'s new
        // `section` / `model` fields exist to catch. Split the
        // stream per pair and assert both pairs actually fired.
        //
        // The two profiles are `[0.5] x 1` and `[0.7] x 1`, so the
        // fan-out is symmetric: each pair contributes
        // `cells * per_cell = 2 * 2 = 4` iterations. The tolerance
        // is +/-1 because the loop's stop condition
        // (`completed + failed >= total`) is evaluated between
        // spawns, so the last pair can be cut one iteration short
        // when a sibling task completes mid-spawn.
        let a = count_iterations_for_pair(&out.events, "mock", "mock-model");
        let b = count_iterations_for_pair(&out.events, "mock", "other-model");
        assert!(
            a > 0 && b > 0,
            "both (section, model) pairs must produce sketches; \
             (mock, mock-model)={a}, (mock, other-model)={b}; stderr={}; events={}",
            out.stderr_text,
            serde_json::to_string_pretty(&out.events).unwrap_or_default()
        );
        assert!(
            a.abs_diff(b) <= 1,
            "the fan-out is symmetric across the two pairs, so their sketch counts \
             must be within 1 of each other; (mock, mock-model)={a}, \
             (mock, other-model)={b}; events={}",
            serde_json::to_string_pretty(&out.events).unwrap_or_default()
        );
        assert_eq!(
            a + b,
            iter_count,
            "every discovery_iteration event must be attributed to one of the two \
             configured pairs; got {a} + {b} != {iter_count}"
        );
    });
}

// ---------------------------------------------------------------------------
// F2 (B1/B2/T2): a v0.14.x-shaped `temperature_profiles` block
// (bare MODEL keys, no `section::` prefix) must upgrade in place
// without ever synthesising a `(section, model)` pair the provider
// registry was not built for.
// ---------------------------------------------------------------------------

/// Write a `config.toml` carrying a v0.14.x-style bare-model
/// `temperature_profiles` entry and return its path. The file is
/// fed to the binary through `MOAGAN_CONFIG`, which
/// `Config::load` honours ahead of every other candidate path.
/// Provider sections are NOT written: `Config::load` merges the
/// built-in defaults (including `mock`) after parsing, so the
/// `--provider mock:mock-model` selection still resolves.
fn write_legacy_profile_config(work: &Path, model_key: &str) -> std::path::PathBuf {
    let path = work.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[discovery_matrix.temperature_profiles.\"{model_key}\"]\n\
             temperatures = [0.5]\n\
             replicas_per_temperature = 1\n"
        ),
    )
    .expect("write config.toml");
    path
}

/// Drive `moagan discover` against a hand-written config whose
/// `[discovery_matrix].temperature_profiles` block uses the
/// v0.14.x bare-model key shape. No `--temperature-profile` flag
/// is passed, so the persisted TOML block is the only source of
/// profiles — the exact path that used to leave the pair out of
/// `active_pairs` and panic at dispatch.
fn drive_legacy_profile_discover(home: &Path, work: &Path, model_key: &str) -> DiscoverOutput {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let config_path = write_legacy_profile_config(work, model_key);
    let stdout_path = work.join("events.jsonl");
    let stderr_path = work.join("log.jsonl");
    let output = Command::new(moagan_bin())
        .env("MOAGAN_HOME", home)
        .env("MOAGAN_CONFIG", &config_path)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .arg("discover")
        .arg("--non-interactive")
        .arg("--prompt")
        .arg("Enumera los 7 colores del arcoiris en orden")
        .arg("--provider")
        .arg("mock:mock-model")
        .arg("--mock-dir")
        .arg(&mock_dir)
        .arg("--matrix-spec")
        .arg("auth=oauth,api-key")
        .arg("--dimensions")
        .arg("1")
        .arg("--sketches-per-cell")
        .arg("2")
        .arg("--max-parallelism")
        .arg("1")
        .arg("--log-format")
        .arg("json")
        .arg("--event-format")
        .arg("jsonl")
        .stdout(std::fs::File::create(&stdout_path).expect("create events.jsonl"))
        .stderr(std::fs::File::create(&stderr_path).expect("create log.jsonl"))
        .output()
        .expect("spawn moagan discover with legacy profile config");
    let exit_code = output.status.code();
    let events = read_jsonl(&stdout_path);
    let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    DiscoverOutput {
        events,
        exit_code,
        stderr_text,
    }
}

/// Read the `temperature_profiles` keys off the
/// `exploration_matrix.json` the coordinator persisted for the
/// (single) run under `MOAGAN_HOME`. The sidecar is written after
/// `migrate_legacy_keys` has run, so its key set is the migration's
/// observable output.
fn persisted_profile_keys(home: &Path) -> Vec<String> {
    let runs = home.join(".runs");
    let mut keys: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&runs)
        .unwrap_or_else(|e| panic!("read {}: {e}", runs.display()))
        .filter_map(|e| e.ok());
    for entry in entries {
        let sidecar = entry.path().join("exploration_matrix.json");
        if !sidecar.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&sidecar)
            .unwrap_or_else(|e| panic!("read {}: {e}", sidecar.display()));
        let value: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("parse {}: {e}", sidecar.display()));
        if let Some(map) = value
            .get("temperature_profiles")
            .and_then(|v| v.as_object())
        {
            keys.extend(map.keys().cloned());
        }
    }
    keys.sort();
    keys
}

/// F2 (T2): a v0.14.x profile keyed by the run's own model name
/// migrates to the joined `section::model` shape, the pair ends up
/// in `active_pairs` (so the registry hosts it), and the fan-out
/// dispatches against it without panicking.
#[test]
fn v0_14_x_bare_model_profile_migrates_to_joined_key() {
    with_moagan_home("discovery_legacy_profile_migrate", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_legacy_profile_discover(home, work.path(), "mock-model");

        assert!(
            !out.stderr_text.contains("panicked at"),
            "the legacy profile must not panic the dispatcher; stderr={}",
            out.stderr_text
        );

        let keys = persisted_profile_keys(home);
        assert!(
            keys.iter().any(|k| k == "mock::mock-model"),
            "the bare `mock-model` key must be re-keyed to `mock::mock-model`; got {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k == "mock-model"),
            "the legacy bare key must not survive the migration; got {keys:?}"
        );

        // The migrated pair is the one the loop dispatched against.
        let migrated = count_iterations_for_pair(&out.events, "mock", "mock-model");
        assert!(
            migrated > 0,
            "the migrated pair must drive the fan-out; events={}; stderr={}",
            serde_json::to_string_pretty(&out.events).unwrap_or_default(),
            out.stderr_text
        );
        assert_eq!(
            migrated,
            count_kind(&out.events, "discovery_iteration"),
            "no iteration may be attributed to any other pair"
        );
    });
}

/// F2 (T2): a v0.14.x profile keyed by some OTHER model is left
/// bare — re-keying it under the run's section would invent a pair
/// (`mock::MiniMax-M3` here, `deepseek::MiniMax-M3` in the report
/// that motivated the fix) that the registry was never asked to
/// build. Whatever the fan-out does with the leftover entry, every
/// `(section, model)` it dispatches against must be a pair the
/// registry hosts, so the run cannot panic in
/// `RunContext::provider_for`.
#[test]
fn v0_14_x_foreign_model_profile_is_not_rekeyed() {
    with_moagan_home("discovery_legacy_profile_foreign", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_legacy_profile_discover(home, work.path(), "MiniMax-M3");

        assert!(
            !out.stderr_text.contains("panicked at"),
            "a foreign legacy profile must not panic the dispatcher; stderr={}",
            out.stderr_text
        );

        let keys = persisted_profile_keys(home);
        assert!(
            keys.iter().any(|k| k == "MiniMax-M3"),
            "the foreign bare key must be preserved verbatim; got {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k == "mock::MiniMax-M3"),
            "the foreign bare key must NOT be re-keyed under the run's section; got {keys:?}"
        );

        // Whatever pairs fired, each must be one the registry
        // hosts: `mock:mock-model` (the `--provider` selection) or
        // a pair derived from the profile map and therefore
        // present in `active_pairs`.
        let iter_events: Vec<&serde_json::Value> = out
            .events
            .iter()
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("discovery_iteration"))
            .collect();
        for ev in &iter_events {
            let section = ev.get("section").and_then(|s| s.as_str()).unwrap_or("");
            let model = ev.get("model").and_then(|m| m.as_str()).unwrap_or("");
            assert_eq!(section, "mock", "unexpected section in {ev}");
            assert!(
                model == "mock-model" || model == "MiniMax-M3",
                "unexpected model in {ev}"
            );
        }
    });
}
