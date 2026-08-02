#!/usr/bin/env bash
# Smoke tests for Phase J (v0.3 «tercera etapa» sub-fase J):
# the new `moagan run --context` plumbing, `moagan continue`,
# `moagan resume`, `moagan rerun`, and `moagan import` subcommands.
#
# The script focuses on the **public CLI surface** and the
# on-disk sidecars. The heavy unit / integration tests live in
# `src/context/{resolver,loader}.rs` and
# `tests/integration_phase_j.rs`.
#
# Each test sets `MOAGAN_HOME` to a fresh tmpdir, runs the CLI,
# and asserts on the artefacts. The script exits non-zero on
# any failure and prints `OK: <test_name>` for every passing
# test. The shell uses `set -uo pipefail` (no `-e`) so a single
# failing test does not abort the whole script; the final exit
# code is derived from the pass/fail counters.
#
# Usage:  ./scripts/smoke_phase_j.sh
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

# ---------------------------------------------------------------------
# 1. Module / file presence
# ---------------------------------------------------------------------

run_test "context_module_layout" '
  [[ -d '"$ROOT"'/src/context ]]
  [[ -f '"$ROOT"'/src/context/mod.rs ]]
  [[ -f '"$ROOT"'/src/context/resolver.rs ]]
  [[ -f '"$ROOT"'/src/context/loader.rs ]]
'

run_test "context_public_api" '
  grep -q "pub enum ContextRef" '"$ROOT"'/src/context/resolver.rs
  grep -q "pub fn resolve_classify" '"$ROOT"'/src/context/resolver.rs
  grep -q "pub fn resolve" '"$ROOT"'/src/context/resolver.rs
  grep -q "pub enum ContextScope" '"$ROOT"'/src/context/loader.rs
  grep -q "pub struct LoadedContext" '"$ROOT"'/src/context/loader.rs
  grep -q "pub struct ContextRefRecord" '"$ROOT"'/src/context/loader.rs
  grep -q "pub fn compute_shared_brief_hash" '"$ROOT"'/src/context/loader.rs
'

run_test "manifest_lineage_fields" '
  grep -q "parent_run_id: Option<RunId>" '"$ROOT"'/src/domain.rs
  grep -q "shared_brief_hash: Option<String>" '"$ROOT"'/src/domain.rs
  grep -q "context_refs: Vec<crate::context::ContextRefRecord>" '"$ROOT"'/src/domain.rs
  grep -q "lineage_paths: Option<LineagePaths>" '"$ROOT"'/src/domain.rs
  grep -q "pub struct LineagePaths" '"$ROOT"'/src/domain.rs
  grep -q "context_block: Option<String>" '"$ROOT"'/src/domain.rs
'

run_test "storage_v007_migration" '
  [[ -f '"$ROOT"'/src/storage/migrations/v007_lineage_context.sql ]]
  grep -q "ALTER TABLE runs ADD COLUMN shared_brief_hash" '"$ROOT"'/src/storage/migrations/v007_lineage_context.sql
  grep -q "ALTER TABLE run_context_refs ADD COLUMN context_type" '"$ROOT"'/src/storage/migrations/v007_lineage_context.sql
  grep -q "ALTER TABLE run_siblings ADD COLUMN relation" '"$ROOT"'/src/storage/migrations/v007_lineage_context.sql
  grep -q "ALTER TABLE run_siblings ADD COLUMN created_unix" '"$ROOT"'/src/storage/migrations/v007_lineage_context.sql
  grep -q "apply_v007_idempotent" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn add_context_ref" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn add_run_sibling_relation" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn last_completed_phase" '"$ROOT"'/src/storage/sqlite.rs
'

run_test "pipeline_resume_helper" '
  grep -q "pub fn canonical_phase_order" '"$ROOT"'/src/phases/pipe.rs
  grep -q "pub fn phase_index" '"$ROOT"'/src/phases/pipe.rs
  grep -q "pub fn resume" '"$ROOT"'/src/phases/pipe.rs
'

run_test "run_context_with_context_builder" '
  grep -q "pub fn with_context" '"$ROOT"'/src/phases/phase.rs
  grep -q "context_block: Option<String>" '"$ROOT"'/src/phases/phase.rs
  grep -q "parent_run_id: Option<RunId>" '"$ROOT"'/src/phases/phase.rs
'

run_test "intake_prepends_context_block" '
  grep -q "context_block" '"$ROOT"'/src/phases/intake.rs
  grep -q "build_user_message" '"$ROOT"'/src/phases/intake.rs
  grep -q "\\[context\\]" '"$ROOT"'/src/phases/intake.rs
'

run_test "cli_run_context_flag" '
  grep -q "context:" '"$ROOT"'/src/cli/mod.rs
  grep -q "context_summary:" '"$ROOT"'/src/cli/mod.rs
  grep -q "context_full:" '"$ROOT"'/src/cli/mod.rs
  grep -q "switch_provider:" '"$ROOT"'/src/cli/mod.rs
  grep -q "switch_api_key:" '"$ROOT"'/src/cli/mod.rs
  grep -q "skip_checkpoint:" '"$ROOT"'/src/cli/mod.rs
  grep -q "matrix_override:" '"$ROOT"'/src/cli/mod.rs
  grep -q "Import {" '"$ROOT"'/src/cli/mod.rs
  grep -q "source_path:" '"$ROOT"'/src/cli/mod.rs
  grep -q "target_runs_dir:" '"$ROOT"'/src/cli/mod.rs
'

run_test "cli_dispatch_wires_new_subcommands" '
  grep -q "Cmd::Import" '"$ROOT"'/src/cli/mod.rs
  grep -q "run_import" '"$ROOT"'/src/cli/mod.rs
  grep -q "ContinueOptions" '"$ROOT"'/src/cli/mod.rs
  grep -q "matrix_override.or(override_json)" '"$ROOT"'/src/cli/mod.rs
'

# ---------------------------------------------------------------------
# 2. End-to-end CLI behaviour (uses a fresh MOAGAN_HOME)
# ---------------------------------------------------------------------

run_test "import_rejects_missing_source" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" import --source-path /tmp/no-such-run-dir 2>&1)
  [[ "$out" == *"source manifest not found"* ]]
'

run_test "rerun_unknown_run_id_errors" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" rerun --run-id 01900000-0000-0000-0000-000000000000 2>&1)
  [[ "$out" == *"not found"* || "$out" == *"Error"* || "$out" == *"error"* ]]
'

run_test "resume_unknown_run_id_errors" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" resume --run-id 01900000-0000-0000-0000-000000000000 2>&1)
  [[ "$out" == *"not found"* || "$out" == *"Error"* || "$out" == *"error"* ]]
'

run_test "continue_unknown_run_id_errors" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" continue --run-id 01900000-0000-0000-0000-000000000000 2>&1)
  [[ "$out" == *"not found"* || "$out" == *"Error"* || "$out" == *"error"* ]]
'

# ---------------------------------------------------------------------
# 3. CLI flag parsing (no execution)
# ---------------------------------------------------------------------

run_test "run_with_context_summary_flag_needs_context" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" run --mode fast --provider mock --prompt "x" --context-summary 2>&1)
  [[ "$out" == *"--context-summary / --context-full require --context"* ]]
'

run_test "run_with_context_full_flag_needs_context" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" run --mode fast --provider mock --prompt "x" --context-full 2>&1)
  [[ "$out" == *"--context-summary / --context-full require --context"* ]]
'

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo
echo "Phase J smoke: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for name in "${FAILED_TESTS[@]}"; do
    echo "  - $name"
  done
  exit 1
fi
exit 0
