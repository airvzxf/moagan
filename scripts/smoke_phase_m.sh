#!/usr/bin/env bash
# Smoke tests for Phase M (v0.4 — error structuring):
# ErrorCode enum + SCREAMING_SNAKE_CASE serialization
# (D.12.8 + D.12.12) and RunPaths::resolve() with relative +
# absolute (D.12.16).
#
# The script focuses on the **on-disk surface** of the
# deliverables. Heavy unit / integration tests live in
# `src/error_code.rs`, `src/error.rs`, `src/fs_layout.rs`, and
# `tests/integration_phase_m.rs` — the smoke checks pin the
# public contract from outside the crate so a future refactor
# that re-shapes the modules still has to leave the CLI /
# filesystem contract intact.
#
# Each check sets `MOAGAN_HOME` to a fresh tmpdir, drives the
# binary where applicable, and asserts on the artefacts. The
# shell uses `set -uo pipefail` (no `-e`) so a single failing
# test does not abort the whole script; the final exit code is
# derived from the pass/fail counters.
#
# Usage:  ./scripts/smoke_phase_m.sh
# Exit:   0 when all checks pass, 1 otherwise.

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

# ---------------------------------------------------------------------
# 1. Module / file presence (D.12.8, D.12.12)
# ---------------------------------------------------------------------

run_test "error_code_module_exists" '
  [[ -f '"$ROOT"'/src/error_code.rs ]]
  grep -q "pub enum ErrorCode" '"$ROOT"'/src/error_code.rs
  grep -q "SCREAMING_SNAKE_CASE" '"$ROOT"'/src/error_code.rs
  grep -q "pub fn stable" '"$ROOT"'/src/error_code.rs
  grep -q "pub fn is_retriable" '"$ROOT"'/src/error_code.rs
  grep -q "pub fn is_circuit_opening" '"$ROOT"'/src/error_code.rs
'

run_test "error_code_has_required_variants" '
  for v in FsNotFound ProviderAuth ProviderRateLimit CheckpointRejected \
           InternalInvariant Http400 Http401 Http403 Http404 Http408 \
           Http413 Http429 Http500 Http502 Http503 Http504 \
           TransportError JsonInvalid SchemaViolation Truncated \
           TimeoutSketch TimeoutPhase TimeoutTotal BudgetExhausted \
           Cancelled PlanPaused CircuitOpen SandboxNotAllowed \
           SandboxTimeout SandboxNoBinary SandboxKilled \
           ProviderOverloaded QuotaExceeded ContentFiltered \
           InvalidResponse NeedsInput ContextRefNotFound \
           ContextRefInvalid InputTooLarge PromptInjectionSuspected \
           PromptInjectionConfirmed HostilePrompt ManifestInconsistent \
           ExportVerificationFailed OutOfDiskSpace UnhandledError; do
    grep -q "    ${v}," '"$ROOT"'/src/error_code.rs \
      || { echo "missing variant $v"; exit 1; }
  done
'

run_test "error_code_wired_into_lib" '
  grep -q "pub mod error_code" '"$ROOT"'/src/lib.rs
'

run_test "error_code_serialization_derives" '
  grep -q "Serialize" '"$ROOT"'/src/error_code.rs
  grep -q "Deserialize" '"$ROOT"'/src/error_code.rs
'

run_test "error_code_method_in_error_rs" '
  grep -q "pub fn code" '"$ROOT"'/src/error.rs
  grep -q "ErrorCode" '"$ROOT"'/src/error.rs
  grep -q "use crate::error_code::ErrorCode" '"$ROOT"'/src/error.rs
'

# ---------------------------------------------------------------------
# 2. RunPaths (D.12.16)
# ---------------------------------------------------------------------

run_test "run_paths_struct_in_fs_layout" '
  grep -q "pub struct RunPaths" '"$ROOT"'/src/fs_layout.rs
  grep -q "pub fn resolve" '"$ROOT"'/src/fs_layout.rs
  grep -q "pub relative:" '"$ROOT"'/src/fs_layout.rs
  grep -q "pub absolute:" '"$ROOT"'/src/fs_layout.rs
  grep -q "Serialize" '"$ROOT"'/src/fs_layout.rs
  grep -q "Deserialize" '"$ROOT"'/src/fs_layout.rs
'

run_test "run_paths_resolves_eight_documented_keys" '
  for k in brief final manifest ranking calls phases warnings checkpoints; do
    grep -q "\"${k}\"" '"$ROOT"'/src/fs_layout.rs \
      || { echo "missing key $k in RunPaths entries"; exit 1; }
  done
'

run_test "manifest_carries_lineage_paths" '
  DOMAIN_FILE=""
  for candidate in '"$ROOT"'/src/domain/mod.rs '"$ROOT"'/src/domain/mod.rs; do
    if [[ -f "$candidate" ]]; then
      DOMAIN_FILE="$candidate"
      break
    fi
  done
  [[ -n "$DOMAIN_FILE" ]] || { echo "no domain module found"; exit 1; }
  grep -q "pub lineage_paths" "$DOMAIN_FILE"
  grep -q "RunPaths" "$DOMAIN_FILE"
  grep -q "skip_serializing_if" "$DOMAIN_FILE"
  # Either RunPaths or LineagePaths (post-J) should appear in the Option type.
  grep -qE "Option<(crate::fs_layout::RunPaths|LineagePaths)>" "$DOMAIN_FILE"
'

run_test "build_manifest_populates_lineage_paths" '
  grep -q "RunPaths::resolve" '"$ROOT"'/src/cli/run.rs
  grep -q "lineage_paths: Some" '"$ROOT"'/src/cli/run.rs
'

# ---------------------------------------------------------------------
# 3. Tests / docs
# ---------------------------------------------------------------------

run_test "error_code_unit_tests_present" '
  for t in stable_matches_serde_json_form \
           stable_returns_screaming_snake_case \
           is_retriable_classification \
           is_circuit_opening_classification \
           serde_round_trip_preserves_value \
           stable_uses_strict_screaming_snake_for_known_codes \
           code_count_is_above_minimum \
           retriable_is_subset_of_circuit_opening_plus_timeouts; do
    grep -q "fn ${t}" '"$ROOT"'/src/error_code.rs \
      || { echo "missing unit test $t"; exit 1; }
  done
'

run_test "error_unit_tests_present" '
  for t in code_maps_every_variant \
           code_serializes_to_screaming_snake_case \
           code_is_consistent_with_policy_helpers \
           code_is_copy; do
    grep -q "fn ${t}" '"$ROOT"'/src/error.rs \
      || { echo "missing unit test $t"; exit 1; }
  done
'

run_test "run_paths_unit_tests_present" '
  for t in run_paths_resolve_returns_both_maps \
           run_paths_resolve_contains_all_documented_keys \
           run_paths_relative_are_run_relative \
           run_paths_absolute_resolve_to_existing_dirs \
           run_paths_round_trips_json; do
    grep -q "fn ${t}" '"$ROOT"'/src/fs_layout.rs \
      || { echo "missing unit test $t"; exit 1; }
  done
'

run_test "integration_phase_m_exists" '
  [[ -f '"$ROOT"'/tests/integration_phase_m.rs ]]
  for t in error_io_returns_code_io \
           error_invalid_args_returns_code_invalid_args \
           error_already_exists_returns_code_already_exists \
           error_code_round_trips_json \
           run_paths_resolves_brief_and_final; do
    grep -q "fn ${t}" '"$ROOT"'/tests/integration_phase_m.rs \
      || { echo "missing integration scenario $t"; exit 1; }
  done
'

# ---------------------------------------------------------------------
# 4. CLI behavior: run a fast mock run + inspect lineage_paths
# ---------------------------------------------------------------------

run_test "fast_mock_run_emits_lineage_paths_in_manifest" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  '"${BIN}"' run --mode fast --provider mock:mock-model --mock-dir '"$ROOT"'/tests/fixtures/mock_provider \
    --prompt "smoke phase M" --non-interactive --runs-dir "${HOME}" >/dev/null 2>&1 \
    || { echo "run failed"; exit 1; }
  # Find the run dir.
  RID=$(ls "${HOME}/.runs" | head -1)
  MANIFEST="${HOME}/.runs/${RID}/manifest.json"
  [[ -f "$MANIFEST" ]] || { echo "no manifest"; exit 1; }
  # lineage_paths must be present and contain both maps.
  grep -q "\"lineage_paths\"" "$MANIFEST" || { echo "no lineage_paths field"; exit 1; }
  grep -q "\"relative\"" "$MANIFEST" || { echo "no relative map"; exit 1; }
  grep -q "\"absolute\"" "$MANIFEST" || { echo "no absolute map"; exit 1; }
  grep -q "\"brief\"" "$MANIFEST" || { echo "no brief key"; exit 1; }
  grep -q "\"final\"" "$MANIFEST" || { echo "no final key"; exit 1; }
  grep -q "\"manifest\"" "$MANIFEST" || { echo "no manifest key"; exit 1; }
  grep -q "\"ranking\"" "$MANIFEST" || { echo "no ranking key"; exit 1; }
  grep -q "\"calls\"" "$MANIFEST" || { echo "no calls key"; exit 1; }
  grep -q "\"phases\"" "$MANIFEST" || { echo "no phases key"; exit 1; }
  grep -q "\"warnings\"" "$MANIFEST" || { echo "no warnings key"; exit 1; }
  grep -q "\"checkpoints\"" "$MANIFEST" || { echo "no checkpoints key"; exit 1; }
  # Absolute brief must point at an existing file.
  RID=$(ls "${HOME}/.runs" | head -1)
  BRIEF_ABS=$(python3 -c "import json; m=json.load(open(\"${MANIFEST}\")); print(m[\"lineage_paths\"][\"absolute\"][\"brief\"])")
  [[ -f "$BRIEF_ABS" ]] || { echo "absolute brief path does not exist: $BRIEF_ABS"; exit 1; }
'

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo
echo "Phase M smoke: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for name in "${FAILED_TESTS[@]}"; do
    echo "  - $name"
  done
  exit 1
fi
exit 0
