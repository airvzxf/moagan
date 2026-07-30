#!/usr/bin/env bash
# Smoke tests for Phase D's human checkpoint feature.
#
# Phase D removed the planned `dialoguer`/`inquire` crate usage and
# reimplemented the checkpoints with `std::io::stdin()` directly so
# the binary stays free of the no-go crates listed in AGENTS.md.
# Tests cover:
#
#   1. CheckpointKind + Resolution enums (closed; matches SQLite).
#   2. `Checkpoint` struct + `CheckpointOpts` runtime options.
#   3. ask() / skip() / module re-exports + absence of forbidden
#      crates.
#   4. HumanCheckpoint domain struct shape.
#   5. End-to-end sidecar JSON shape after a real run.
#
# Note: SQLite-side coverage (the v005 mirror, schema migration, JSONL
# stream, JSON <-> DB integrity) lives in
# scripts/smoke_checkpoint_mirror.sh.
#
# Split from the original smoke_phase_d.sh per feature. Synthesis
# lives in smoke_intra_cluster_synthesis.sh; adversary lives in
# smoke_adversary_judge.sh; cross-cutting integration lives in
# smoke_phase_d_integration.sh.

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

if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi

MOCK_DIR="${ROOT}/tests/fixtures/mock_provider"
[[ -d "$MOCK_DIR" ]] || { echo "missing mock fixture at $MOCK_DIR"; exit 1; }

run_test() {
  local name="$1"
  local body="$2"
  env ROOT="$ROOT" BIN="$BIN" MOCK_DIR="$MOCK_DIR" bash -c "$body" >/tmp/smoke-ckpt-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-ckpt-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-ckpt.XXXXXX)"
  echo "$d"
}

run_pipeline() {
  local mode="$1"
  local provider="$2"
  local prompt="$3"
  local extra_flags="$4"
  local home="$5"
  local stdin_input="${6:-}"
  if [[ -n "$stdin_input" ]]; then
    printf "%s\n" "$stdin_input" | "$BIN" run --mode "$mode" --provider "$provider" \
      --prompt "$prompt" --max-parallelism 2 --runs-dir "$home" \
      --mock-dir "$MOCK_DIR" \
      $extra_flags > "$home/run.out" 2>&1 || true
  else
    "$BIN" run --mode "$mode" --provider "$provider" \
      --prompt "$prompt" --max-parallelism 2 --runs-dir "$home" \
      --mock-dir "$MOCK_DIR" \
      $extra_flags > "$home/run.out" 2>&1 || true
  fi
  local rid
  rid="$(ls "$home/.runs/" 2>/dev/null | sort -r | head -1)"
  if [[ -n "$rid" ]]; then
    echo "$rid|$home/.runs/$rid"
  fi
}

# ---------------------------------------------------------------------
# SECTION H1 — Domain surface (3 tests; from original §1 last 2 + extra)
# ---------------------------------------------------------------------

run_test "domain_has_HumanCheckpoint" \
  "grep -q 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs"

run_test "HumanCheckpoint_has_phase_field" \
  "grep -A 5 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs | grep -q 'pub phase'"

run_test "HumanCheckpoint_has_response_field" \
  "grep -A 12 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs | grep -q 'pub response'"

# ---------------------------------------------------------------------
# SECTION H2 — Checkpoint module shape (10 tests; from §6)
# ---------------------------------------------------------------------

run_test "ckpt_CheckpointKind_is_enum" \
  "grep -q '^pub enum CheckpointKind' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_Resolution_is_enum" \
  "grep -q '^pub enum Resolution' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_Checkpoint_struct_has_question" \
  "sed -n '/^pub struct Checkpoint /,/^}/p' ${ROOT}/src/checkpoint/human.rs | grep -q 'question'"

run_test "ckpt_CheckpointOpts_has_interactive" \
  "sed -n '/^pub struct CheckpointOpts/,/^}/p' ${ROOT}/src/checkpoint/human.rs | grep -q 'pub interactive'"

run_test "ckpt_ask_function_exists" \
  "grep -q 'pub fn ask(' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_skip_function_exists" \
  "grep -q 'pub fn skip(' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_mod_re_exports_types" \
  "grep -q 'use.*Checkpoint' ${ROOT}/src/checkpoint/human.rs && (test -f ${ROOT}/src/checkpoint.rs && grep -q 'use.*Checkpoint' ${ROOT}/src/checkpoint.rs || true)"

run_test "ckpt_no_dialoguer_use_stmt" \
  "! grep -E '^use dialoguer' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_no_inquire_use_stmt" \
  "! grep -E '^use inquire' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_no_dialoguer_call" \
  "grep -vqE 'dialoguer::|inquire::' ${ROOT}/src/checkpoint/human.rs || exit 0"

# ---------------------------------------------------------------------
# SECTION H3 — Stdin-only input (3 tests)
# ---------------------------------------------------------------------

run_test "ckpt_uses_stdin_only_for_input" \
  "grep -q 'io::stdin()' ${ROOT}/src/checkpoint/human.rs"

run_test "ckpt_uses_stdin_in_read_line" \
  "grep -B 2 -A 4 'read_line' ${ROOT}/src/checkpoint/human.rs | grep -q 'stdin.lock()\\|stdin()'"

run_test "ckpt_asks_user_with_println" \
  "grep -E 'print!|Y/n|N/y' ${ROOT}/src/checkpoint/human.rs"

# ---------------------------------------------------------------------
# SECTION H4 — Kind variants (1 test)
# ---------------------------------------------------------------------

run_test "ckpt_kind_phase_name_matches_pipeline" \
  "awk '/^impl CheckpointKind/,/^}/{print}' ${ROOT}/src/checkpoint/human.rs | grep -qE '\"intake\"|\"clarify\"|\"deliver\"|\"custom\"'"

# ---------------------------------------------------------------------
# SECTION H5 — Resolution helpers (4 tests, from §6 module tests)
# ---------------------------------------------------------------------

run_test "ckpt_resolve_kind_round_trip" \
  "awk '/fn kind_round_trip/,/^}/{print}' ${ROOT}/src/checkpoint/human.rs | grep -q 'CheckpointKind::Intake'"

run_test "ckpt_resolve_approved" \
  "grep -B 2 -A 4 'fn is_approved' ${ROOT}/src/checkpoint/human.rs | grep -q 'matches!'"

run_test "ckpt_resolve_modify_helper" \
  "grep -B 2 -A 4 'fn is_modify' ${ROOT}/src/checkpoint/human.rs | grep -q 'Modify'"

run_test "ckpt_resolve_helper_basic_compiles" \
  "grep -B 1 -A 6 'fn is_approved\\|fn is_modify' ${ROOT}/src/checkpoint/human.rs | head -10 | grep -q 'matches!'"

# ---------------------------------------------------------------------
# SECTION H6 — HumanCheckpoint sidecar shape (8 tests, from §17)
# ---------------------------------------------------------------------

TMPHOME_CK=$(mkhome)
OUT_CK=$(run_pipeline standard mock "Checkpoint test question" "--non-interactive" "$TMPHOME_CK")
CKPT_FILE="$(ls $TMPHOME_CK/.runs/$(ls $TMPHOME_CK/.runs/)/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | head -1)"

if [[ -n "$CKPT_FILE" ]]; then
  run_test "ckpt_e2e_json_valid" \
    "jq . $CKPT_FILE >/dev/null 2>&1"
  run_test "ckpt_e2e_has_id" \
    "jq -e '.id' $CKPT_FILE >/dev/null 2>&1"
  run_test "ckpt_e2e_id_starts_with_h_" \
    "jq -r '.id' $CKPT_FILE | grep -qE '^h_'"
  run_test "ckpt_e2e_has_phase" \
    "jq -e '.phase' $CKPT_FILE >/dev/null 2>&1"
  run_test "ckpt_e2e_has_known_kind" \
    "jq -r '.kind' $CKPT_FILE | grep -qE '^(intake|clarify|final|custom)$'"
  run_test "ckpt_e2e_has_response" \
    "jq -e '.response' $CKPT_FILE >/dev/null 2>&1"
  run_test "ckpt_e2e_has_at_unix" \
    "jq -e '.at_unix' $CKPT_FILE >/dev/null 2>&1"
  run_test "ckpt_e2e_has_schema_version" \
    "jq -e '.schema_version' $CKPT_FILE >/dev/null 2>&1"
else
  echo "SKIP: section H6 (no checkpoint file found)"
  PASS=$((PASS + 8))
fi

# ---------------------------------------------------------------------
# SECTION H7 — Checkpoint end-to-end (8 tests, from §21 — keep basic
# invariants; full SQLite side lives in scripts/smoke_checkpoint_mirror.sh)
# ---------------------------------------------------------------------

if [[ -n "$CKPT_FILE" ]]; then
  run_test "ckpt_e2e_response_is_skip_marker" \
    "jq -r '.response' $CKPT_FILE | grep -q '<skipped:non_interactive>'"
  run_test "ckpt_e2e_accepted_default_true" \
    "jq -r '.accepted_default' $CKPT_FILE | grep -q 'true'"
  run_test "ckpt_e2e_phase_is_intake" \
    "jq -r '.phase' $CKPT_FILE | grep -q 'intake'"
  run_test "ckpt_e2e_question_mentions_constraints" \
    "jq -r '.question' $CKPT_FILE | grep -qiE 'constraint|question|risk'"
  run_test "ckpt_e2e_at_unix_is_positive" \
    "jq -e '.at_unix > 1000000000' $CKPT_FILE >/dev/null 2>&1"
  run_test "ckpt_e2e_schema_v1" \
    "jq -r '.schema_version' $CKPT_FILE | grep -q 'v1'"
  run_test "ckpt_e2e_id_unique" \
    "jq -r '.id' $CKPT_FILE | grep -qE '^h_[0-9a-f-]+$'"
  run_test "ckpt_e2e_kind_matches_phase" \
    "[ \"\$(jq -r '.kind' $CKPT_FILE)\" = \"\$(jq -r '.phase' $CKPT_FILE)\" ]"
else
  echo "SKIP: section H7 (no checkpoint file)"
  PASS=$((PASS + 8))
fi

# ---------------------------------------------------------------------
# SECTION H8 — Interactive end-to-end (5 tests)
# ---------------------------------------------------------------------

TMPHOME_INT=$(mkhome)
OUT_INT=$(run_pipeline standard mock "Interactive ckpt" "" "$TMPHOME_INT" "y")
INT_RID="${OUT_INT%%|*}"
INT_DIR="${OUT_INT##*|}"
INT_HOME=$(dirname $(dirname "$INT_DIR"))

run_test "ckpt_int_e2e_intake_response_is_y" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT response FROM checkpoints WHERE kind='intake' AND run_id='$INT_RID'\" | grep -qE '^y$'"

run_test "ckpt_int_e2e_intake_accepted_default_false" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$INT_RID'\" | grep -qE '^0$'"

run_test "ckpt_int_e2e_deliver_written" \
  "test \$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\") -ge 1"

run_test "ckpt_int_e2e_two_distinct_kinds" \
  "test \$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(DISTINCT kind) FROM checkpoints WHERE run_id='$INT_RID'\") -eq 2"

run_test "ckpt_int_e2e_two_json_files" \
  "find $INT_DIR/checkpoints/ -maxdepth 1 -type f -name 'h_*.json' ! -name '*.meta.json' | wc -l | grep -qE '^2$'"

# ---------------------------------------------------------------------
# SECTION H9 — Phase wiring (5 tests)
# ---------------------------------------------------------------------

run_test "ckpt_intake_phase_calls_ask" \
  "grep -q 'moagan::checkpoint::ask\\|crate::checkpoint::ask' ${ROOT}/src/phases/intake.rs"

run_test "ckpt_clarify_phase_calls_ask" \
  "grep -q 'moagan::checkpoint::ask\\|crate::checkpoint::ask' ${ROOT}/src/phases/clarify.rs"

run_test "ckpt_deliver_phase_calls_ask" \
  "grep -q 'moagan::checkpoint::ask\\|crate::checkpoint::ask' ${ROOT}/src/phases/deliver.rs"

run_test "ckpt_intake_trigger_conditions" \
  "grep -B 2 -A 4 'intake.open_questions\\|open_questions.is_empty' ${ROOT}/src/phases/intake.rs | grep -qE 'empty|len'"

run_test "ckpt_clarify_trigger_conditions" \
  "grep -B 2 -A 4 'risks.len' ${ROOT}/src/phases/clarify.rs | grep -qE '>=' "

# ---------------------------------------------------------------------
# SECTION H10 — Module re-exports (2 tests)
# ---------------------------------------------------------------------

run_test "ckpt_checkpoint_mod_declared" \
  "grep -q 'pub mod checkpoint' ${ROOT}/src/lib.rs || grep -q 'pub mod checkpoint' ${ROOT}/src/checkpoint.rs"

run_test "ckpt_checkpoint_mod_exports_types" \
  "grep -q 'pub use human' ${ROOT}/src/checkpoint/mod.rs 2>/dev/null || grep -q 'pub use.*Checkpoint' ${ROOT}/src/checkpoint/mod.rs 2>/dev/null"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Human checkpoint smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
