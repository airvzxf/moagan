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
//!    × `--sketches-per-cell 10` (the F2 floor) ×
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

/// The nine wire-schema fields `Event::DiscoveryIteration` MUST
/// carry on every emit (per `docs/events-v1.md` line 103 and
/// the `Event::DiscoveryIteration` variant in
/// `src/telemetry/stdout_events.rs`). The set is duplicated here
/// on purpose — a future schema drop surfaces as a missing-field
/// assertion rather than silently shipping a wire-format change.
const ITER_REQUIRED_FIELDS: &[&str] = &[
    "schema",
    "ts",
    "n",
    "total",
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
/// makes the per-cell fan-out = 10 (the F2 floor). With
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
