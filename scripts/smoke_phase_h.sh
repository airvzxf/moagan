#!/usr/bin/env bash
# Smoke tests for Phase H (v0.3 «tercera etapa»): the ranking
# stability check (V4 §5.12 paso 6), the proposal.source_nodes
# populate (Phase G limitation #2 follow-up), and the V4 §5.14
# "el ranking es inestable" human-checkpoint trigger.
#
# The script focuses on the **public CLI surface** and the on-disk
# sidecars. The heavy unit / integration tests live in
# src/ranking/stability.rs and tests/integration_phase_h.rs.
#
# Each test sets MOAGAN_HOME to a fresh tmpdir, runs the CLI,
# and asserts on the artefacts. The script exits non-zero on
# any failure and prints `OK: <test_name>` for every passing
# test. The shell uses `set -uo pipefail` (no `-e`) so a single
# failing test does not abort the whole script; the final exit
# code is derived from the pass/fail counters.
#
# Usage:  ./scripts/smoke_phase_h.sh
# Exit:   0 when all tests pass, 1 otherwise.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${ROOT}/target/debug/moagan"
PASS=0
FAIL=0
FAILED_TESTS=()

if [[ ! -x "$BIN" ]]; then
  echo "moagan binary not built at $BIN; run 'cargo build' first"
  exit 1
fi

# ---------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------

run_test() {
  local name="$1"
  local body="$2"
  bash -c "$body" >/tmp/smoke-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

assert_file_exists() {
  local path="$1"
  [[ -f "$path" ]] || { echo "expected file to exist: $path" >&2; return 1; }
}

assert_contains() {
  local path="$1"
  local needle="$2"
  if ! grep -qF "$needle" "$path"; then
    echo "expected $path to contain: $needle" >&2
    return 1
  fi
}

TMPDIR_H="$(mktemp -d)"
trap "rm -rf '$TMPDIR_H'" EXIT

# ---------------------------------------------------------------------
# 1. Module / file presence
# ---------------------------------------------------------------------

run_test "stability_module_present" '
  [[ -f '"$ROOT"'/src/ranking/stability.rs ]]
'

run_test "stability_re_exported_in_ranking_mod" '
  grep -q "pub mod stability;" '"$ROOT"'/src/ranking/mod.rs
'

run_test "stability_config_in_config_rs" '
  grep -q "pub struct StabilityConfig" '"$ROOT"'/src/config.rs
  grep -q "pub stability: StabilityConfig" '"$ROOT"'/src/config.rs
'

run_test "ranking_stability_fields_in_domain" '
  grep -q "stability_score" '"$ROOT"'/src/domain/mod.rs
  grep -q "stability_label" '"$ROOT"'/src/domain/mod.rs
  grep -q "stability_sigma" '"$ROOT"'/src/domain/mod.rs
  grep -q "pub enum StabilityLabel" '"$ROOT"'/src/domain/mod.rs
'

run_test "fs_layout_problem_graph_helper" '
  grep -q "fn problem_grid" '"$ROOT"'/src/fs_layout.rs && \
    echo "no problem_graph fn" >&2 && exit 1
  grep -q "pub fn problem_graph" '"$ROOT"'/src/fs_layout.rs
'

run_test "rank_phase_has_stability_enabled_field" '
  grep -q "stability_enabled: bool" '"$ROOT"'/src/phases/rank.rs
'

run_test "rank_phase_invokes_stability_check" '
  grep -q "stability_check(" '"$ROOT"'/src/phases/rank.rs
  grep -q "StabilityLabel" '"$ROOT"'/src/phases/rank.rs
'

run_test "propose_phase_populates_source_nodes" '
  grep -q "compute_source_nodes" '"$ROOT"'/src/phases/propose.rs
  grep -q "load_problem_graph" '"$ROOT"'/src/phases/propose.rs
'

run_test "rank_phase_fires_checkpoint_on_sensitive" '
  grep -q "CheckpointKind::Custom" '"$ROOT"'/src/phases/rank.rs
  grep -q "stability_label == Some(StabilityLabel::Sensitive)" '"$ROOT"'/src/phases/rank.rs
'

# ---------------------------------------------------------------------
# 2. Library tests for stability + source_nodes pass
# ---------------------------------------------------------------------

run_test "lib_stability_tests_pass" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --lib ranking::stability:: --quiet) >/dev/null
'

run_test "lib_propose_source_nodes_tests_pass" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --lib phases::propose::tests:: --quiet) >/dev/null
'

run_test "lib_rank_phase_tests_pass" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --lib phases::rank:: --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 3. CI gates: no Anthropic SDK + no forbidden crates
# ---------------------------------------------------------------------

run_test "no_anthropic_sdk_in_source" '
  bash '"$ROOT"'/scripts/check-no-anthropic-sdk.sh
'

run_test "forbidden_crates_check_passes" '
  bash '"$ROOT"'/scripts/check-no-forbidden-crates.sh
'

# ---------------------------------------------------------------------
# 4. End-to-end: standard run writes ranking.json with stability fields
# ---------------------------------------------------------------------

MOCK_DIR="${ROOT}/tests/fixtures/mock_provider"

run_test "cli_standard_writes_ranking_with_stability_fields" '
  export MOAGAN_HOME="'"$TMPDIR_H"'/standard"
  mkdir -p "$MOAGAN_HOME"
  "'"$BIN"'" run --mode standard --provider mock:mock-model \
    --mock-dir "'"$MOCK_DIR"'" \
    --prompt "Enumera los 7 colores del arcoiris en orden" \
    --non-interactive --max-parallelism 2 >/dev/null 2>&1 || true
  [[ -d "$MOAGAN_HOME/.runs" ]] || { echo "no runs dir" >&2; exit 1; }
  run_dir=$(ls -1 "$MOAGAN_HOME/.runs" 2>/dev/null | head -1)
  [[ -n "$run_dir" ]] || { echo "no run dir" >&2; exit 1; }
  ranking="$MOAGAN_HOME/.runs/$run_dir/rankings/ranking.json"
  [[ -f "$ranking" ]] || { echo "missing ranking.json" >&2; exit 1; }
  grep -qF "\"stability_score\"" "$ranking" || { echo "missing stability_score" >&2; exit 1; }
  grep -qF "\"stability_label\"" "$ranking" || { echo "missing stability_label" >&2; exit 1; }
  grep -qF "\"stability_sigma\"" "$ranking" || { echo "missing stability_sigma" >&2; exit 1; }
'

run_test "cli_fast_writes_ranking_with_stability_fields" '
  export MOAGAN_HOME="'"$TMPDIR_H"'/fast"
  mkdir -p "$MOAGAN_HOME"
  "'"$BIN"'" run --mode fast --provider mock:mock-model \
    --mock-dir "'"$MOCK_DIR"'" \
    --prompt "Enumera los 7 colores del arcoiris en orden" \
    --non-interactive --max-parallelism 2 >/dev/null 2>&1 || true
  [[ -d "$MOAGAN_HOME/.runs" ]] || { echo "no runs dir" >&2; exit 1; }
  run_dir=$(ls -1 "$MOAGAN_HOME/.runs" 2>/dev/null | head -1)
  [[ -n "$run_dir" ]] || { echo "no run dir" >&2; exit 1; }
  ranking="$MOAGAN_HOME/.runs/$run_dir/rankings/ranking.json"
  [[ -f "$ranking" ]] || { echo "missing ranking.json" >&2; exit 1; }
  grep -qF "\"stability_score\"" "$ranking" || { echo "missing stability_score" >&2; exit 1; }
'

# ---------------------------------------------------------------------
# 5. End-to-end: legacy ranking sidecars parse cleanly
# ---------------------------------------------------------------------

# Write a sidecar with only the v0.2 shape (no stability fields) and
# confirm `moagan inspect` accepts it without choking. The mock runs
# don't expose inspect semantics for ranking.json, so this test uses
# python to read the JSON; the library code path that actually
# matters is covered by tests/integration_phase_h.rs::legacy_ranking_...
run_test "legacy_ranking_json_parses_via_lib" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h legacy_ranking_without_stability_fields_parses --quiet) >/dev/null
'

run_test "ranking_with_stability_round_trips_via_lib" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h ranking_with_stability_round_trips_json --quiet) >/dev/null
'

run_test "rank_phase_integration_tests_all_pass" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 6. Default sigma does NOT flip a clear winner — ranking labels Stable
# ---------------------------------------------------------------------

run_test "default_sigma_labels_clear_winner_stable" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h ranking_marked_stable_when_weights_uniform_and_clear_winner --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 7. Wide sigma + split criteria labels Sensitive
# ---------------------------------------------------------------------

run_test "high_sigma_labels_split_criteria_sensitive" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h ranking_marked_sensitive_under_high_sigma_perturbation --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 8. Disabled stability keeps the sidecar fields absent
# ---------------------------------------------------------------------

run_test "disabled_stability_keeps_fields_absent" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h ranking_stability_fields_absent_when_disabled --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 9. V4 §5.14 trigger fires a checkpoint on Sensitive
# ---------------------------------------------------------------------

run_test "interactive_sensitive_run_writes_checkpoint_sidecar" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h human_checkpoint_triggered_on_sensitive_interactive_run --quiet) >/dev/null
'

run_test "non_interactive_sensitive_run_writes_skip_marker" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_h non_interactive_sensitive_run_writes_skip_marker --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 10. Phase G limitation #2 follow-up: source_nodes populate
# ---------------------------------------------------------------------

run_test "proposal_source_nodes_populate_unit_test" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --lib phases::propose::tests::source_nodes_picks_up_high_overlap_nodes --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo
echo "Phase H smoke: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  for t in "${FAILED_TESTS[@]}"; do
    echo "  - $t"
  done
  exit 1
fi