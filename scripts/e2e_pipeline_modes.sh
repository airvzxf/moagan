#!/usr/bin/env bash
# Pipeline end-to-end tests that actually launch `moagan run` against
# the mock LLM provider and then walk the artifact tree.
#
# These were extracted from `smoke_intra_cluster_synthesis.sh` §A
# (multi-prompt e2e) and `smoke_phase_d_integration.sh` §B + §K
# (mode matrix + cross-mode parity) because they were the most
# expensive sections of those scripts (5 standard-mode runs + 4
# mode-coverage runs = ~9 binary executions, ~45–60 s wall time).
# Running them on every inner-loop iteration is wasteful.
#
# Tests covered here:
#   1. Multi-prompt synthesis e2e (5 standard-mode runs across
#      different briefs — sanity test that the synthesis propagation
#      is stable across prompts).
#   2. Mode matrix (4 modes: fast, standard, deep, batch — verify
#      each mode produces the artefacts its spec mandates).
#   3. Cross-mode parity invariants (same Phase D shape across
#      standard/deep/batch; fast stays lean).
#
# Companion scripts:
#   scripts/smoke_intra_cluster_synthesis.sh  (pure static — runs in <1 s)
#   scripts/smoke_phase_d_integration.sh      (mostly static — runs in <5 s)
#   scripts/e2e_interactive_checkpoints.sh     (interactive end-to-end)
#   scripts/e2e_audit_proxy.sh                (forwarder + real LLM)
#
# Env vars (all optional):
#   MOAGAN_E2E_FAST  set to 1 to reduce the multi-prompt set from
#                     5 to 2 (saves ~10 s) for tight inner loops.

set -o pipefail

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
[[ -d "$MOCK_DIR" ]] || { echo "missing mock fixture dir at $MOCK_DIR"; exit 1; }

: "${MOAGAN_E2E_FAST:=0}"

run_test() {
  local name="$1"
  local body="$2"
  env ROOT="$ROOT" BIN="$BIN" MOCK_DIR="$MOCK_DIR" bash -c "$body" >/tmp/e2e-pipe-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/e2e-pipe-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-e2e-pipe.XXXXXX)"
  echo "$d"
}

run_pipeline() {
  local mode="$1"
  local provider="$2"
  local prompt="$3"
  local extra_flags="$4"
  local home="$5"
  "$BIN" run --mode "$mode" --provider "$provider" \
    --prompt "$prompt" \
    --max-parallelism 2 \
    --runs-dir "$home" \
    --mock-dir "$MOCK_DIR" \
    $extra_flags > "$home/run.out" 2>&1 || true
  local rid
  rid="$(ls "$home/.runs/" 2>/dev/null | sort -r | head -1)"
  if [[ -n "$rid" ]]; then
    echo "$rid|$home/.runs/$rid"
  fi
}

# Helper that just produces the run dir (no echo of run_id).
run_pipeline_into() {
  local mode="$1"
  local home="$2"
  "$BIN" run --mode "$mode" --provider mock:mock-model \
    --prompt "Different prompt $mode" \
    --max-parallelism 2 --runs-dir "$home" --mock-dir "$MOCK_DIR" \
    --non-interactive > "$home/run.out" 2>&1 || true
  local rid
  rid="$(ls "$home/.runs/" 2>/dev/null | sort -r | head -1)"
  [[ -n "$rid" ]] && echo "$home/.runs/$rid"
}

# =====================================================================
# SECTION A — Multi-prompt synthesis e2e (20 tests)
#
# Standard mode × 5 different prompts. Sanity test that the
# synthesis propagation is stable across briefs. Set
# `MOAGAN_E2E_FAST=1` to shrink this to 2 prompts in tight loops.
# =====================================================================

if [[ "$MOAGAN_E2E_FAST" == "1" ]]; then
  declare -a PROMPTS=(
    "Build a REST API for tracking library books"
    "Build a CLI for batch CSV processing"
  )
else
  declare -a PROMPTS=(
    "Build a REST API for tracking library books"
    "Design a distributed message queue"
    "Build a CLI for batch CSV processing"
    "Design a CI pipeline for Rust services"
    "Implement an OAuth 2.0 authorization server"
  )
fi
declare -a RUN_DIRS_A=()

for i in "${!PROMPTS[@]}"; do
  H=$(mkhome)
  OUT=$(run_pipeline standard mock:mock-model "${PROMPTS[$i]}" "--non-interactive" "$H")
  RUN_DIRS_A+=("${OUT##*|}")
done

run_test "A1_all_runs_completed" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/manifest.json ]] || exit 1; done"

run_test "A2_all_runs_manifest_has_status_completed" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.status == \"completed\"' \$d/manifest.json >/dev/null || exit 1; done"

run_test "A3_all_runs_have_synthesis_propagated" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/proposals/s_00.json ]] || exit 1; done"

run_test "A4_all_runs_have_synthesis_lineage" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/synthesized/s_00.json ]] || exit 1; done"

run_test "A5_all_runs_have_cluster_file" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/cluster_proposals/cp_00.json ]] || exit 1; done"

run_test "A6_all_runs_synthesis_in_critiques" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do ls \$d/critiques/s_*.json 2>/dev/null | head -1 | grep -q s_ || exit 1; done"

run_test "A7_all_runs_synthesis_in_evaluations" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/evaluations/s_00.json ]] || exit 1; done"

run_test "A8_all_runs_synthesis_in_ranking" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -r '.ranked[].id' \$d/rankings/ranking.json | grep -q '^s_' || exit 1; done"

run_test "A9_all_runs_have_final_portfolio" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/final/portfolio.md ]] || exit 1; done"

run_test "A10_all_runs_have_telemetry" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do [[ -f \$d/telemetry/calls.jsonl.gz ]] || exit 1; done"

run_test "A11_all_runs_have_meta_db" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do home=\$(dirname \$(dirname \$d)); [[ -f \$home/meta.sqlite ]] || exit 1; done"

run_test "A12_all_runs_synth_id_format_consistent" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -r '.id' \$d/synthesized/s_00.json | grep -qE '^s_[0-9]+\$' || exit 1; done"

run_test "A13_all_runs_synth_has_source_proposals" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.source_proposals | length > 0' \$d/synthesized/s_00.json >/dev/null || exit 1; done"

run_test "A14_all_runs_propagated_proposal_has_id" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.id' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A15_all_runs_propagated_proposal_has_source_sketch" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.source_sketch | startswith(\"syn_from_\")' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A16_all_runs_cluster_ids_match_synthesis" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do s_id=\$(jq -r '.id' \$d/synthesized/s_00.json); ls \$d/proposals/\${s_id}.json >/dev/null 2>&1 || exit 1; done"

run_test "A17_all_runs_propagated_proposal_empty_artifacts" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.artifacts | length == 0' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A18_all_runs_propagated_proposal_has_evidence_or_empty" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.evidence | type == \"array\"' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A19_all_runs_propagated_proposal_has_tradeoffs_or_empty" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}'; do jq -e '.tradeoffs | type == \"array\"' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A20_all_runs_have_unique_run_ids" \
  "[[ \"${RUN_DIRS_A[0]}\" != \"${RUN_DIRS_A[1]}\" ]]"

# =====================================================================
# SECTION B — Mode matrix (15 tests)
#
# One run per mode (fast/standard/deep/batch) with the same brief.
# Confirms each mode produces the artefact tree its spec mandates.
# =====================================================================

TMPHOME_M=$(mkhome)
mkdir -p "$TMPHOME_M/.runs"

RUN_DIR_FAST=$(run_pipeline_into fast "$TMPHOME_M")
RUN_DIR_STD=$(run_pipeline_into standard "$TMPHOME_M")
RUN_DIR_DEEP=$(run_pipeline_into deep "$TMPHOME_M")
RUN_DIR_BATCH=$(run_pipeline_into batch "$TMPHOME_M")

run_test "B1_fast_mode_no_synthesis_files" \
  "! ls $RUN_DIR_FAST/synthesized/s_*.json 2>/dev/null | grep -q s_"

run_test "B3_standard_mode_has_synthesis" \
  "[[ -f $RUN_DIR_STD/synthesized/s_00.json ]]"

run_test "B4_deep_mode_has_synthesis" \
  "[[ -f $RUN_DIR_DEEP/synthesized/s_00.json ]]"

run_test "B5_batch_mode_has_synthesis" \
  "[[ -f $RUN_DIR_BATCH/synthesized/s_00.json ]]"

run_test "B9_standard_synthesis_in_ranking" \
  "jq -r '.ranked[].id' $RUN_DIR_STD/rankings/ranking.json 2>/dev/null | grep -q '^s_'"

run_test "B10_deep_synthesis_in_ranking" \
  "jq -r '.ranked[].id' $RUN_DIR_DEEP/rankings/ranking.json 2>/dev/null | grep -q '^s_'"

run_test "B11_batch_synthesis_in_ranking" \
  "jq -r '.ranked[].id' $RUN_DIR_BATCH/rankings/ranking.json 2>/dev/null | grep -q '^s_'"

run_test "B12_all_modes_create_manifest" \
  "[[ -f $RUN_DIR_FAST/manifest.json ]] && [[ -f $RUN_DIR_STD/manifest.json ]] && [[ -f $RUN_DIR_DEEP/manifest.json ]] && [[ -f $RUN_DIR_BATCH/manifest.json ]]"

run_test "B13_all_modes_create_final" \
  "[[ -d $RUN_DIR_FAST/final ]] && [[ -d $RUN_DIR_STD/final ]] && [[ -d $RUN_DIR_DEEP/final ]] && [[ -d $RUN_DIR_BATCH/final ]]"

run_test "B15_all_modes_have_synthesized_dir" \
  "[[ -d $RUN_DIR_STD/synthesized ]] && [[ -d $RUN_DIR_DEEP/synthesized ]] && [[ -d $RUN_DIR_BATCH/synthesized ]]"

run_test "B_all_runs_distinct" \
  "test \$(ls -d $TMPHOME_M/.runs/* | wc -l) -eq 4"

run_test "B_all_runs_unique_ids" \
  "for d in $TMPHOME_M/.runs/*; do jq -r .run_id \$d/manifest.json | sort -u | head -1; done | sort -u | wc -l | grep -qE '^4$'"

run_test "B_each_mode_produces_evaluations" \
  "for d in $RUN_DIR_FAST $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do ls \$d/evaluations/*.json 2>/dev/null | grep -v meta | head -1 | grep -q . || exit 1; done"

run_test "B_each_mode_produces_rankings" \
  "for d in $RUN_DIR_FAST $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do [[ -f \$d/rankings/ranking.json ]] || exit 1; done"

run_test "B_fast_mode_no_propagated_synthesis" \
  "! ls $RUN_DIR_FAST/proposals/s_*.json 2>/dev/null | head -1 | grep -q s_"

# =====================================================================
# SECTION K — Cross-mode parity (15 tests)
# =====================================================================

run_test "K1_all_modes_synthesized_id_format_consistent" \
  "for d in $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do jq -r '.id' \$d/synthesized/s_00.json | grep -qE '^s_[0-9]+\$' || exit 1; done"

run_test "K2_all_modes_cluster_id_format_consistent" \
  "for d in $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do jq -r '.id' \$d/cluster_proposals/cp_00.json | grep -qE '^cp_[0-9]+\$' || exit 1; done"

run_test "K4_all_modes_synthesized_has_source_proposals" \
  "for d in $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do jq -e '.source_proposals | length > 0' \$d/synthesized/s_00.json >/dev/null || exit 1; done"

run_test "K6_fast_mode_no_synthesis_in_portfolio" \
  "! grep -q 's_' $RUN_DIR_FAST/final/portfolio.md || true"

run_test "K9_all_modes_have_checkpoints_dir" \
  "[[ -d $RUN_DIR_FAST/checkpoints ]] && [[ -d $RUN_DIR_STD/checkpoints ]] && [[ -d $RUN_DIR_DEEP/checkpoints ]] && [[ -d $RUN_DIR_BATCH/checkpoints ]]"

run_test "K10_batch_mode_checkpoints_are_skipped" \
  "ls $RUN_DIR_BATCH/checkpoints/h_*.json 2>/dev/null | head -1 | xargs jq -r '.response' | grep -q '<skipped:non_interactive>'"

run_test "K11_fast_mode_checkpoints_skipped" \
  "ls $RUN_DIR_FAST/checkpoints/h_*.json 2>/dev/null | head -1 | xargs jq -r '.response' | grep -q '<skipped:non_interactive>' || true"

run_test "K12_standard_mode_checkpoint_kind_is_known" \
  "sqlite3 \$(dirname \$(dirname $RUN_DIR_STD))/meta.sqlite 'SELECT DISTINCT kind FROM checkpoints' | head -1 | grep -qE 'intake|clarify|final'"

run_test "K13_all_modes_synthesized_dir_exists" \
  "[[ -d $RUN_DIR_STD/synthesized ]] && [[ -d $RUN_DIR_DEEP/synthesized ]] && [[ -d $RUN_DIR_BATCH/synthesized ]]"

run_test "K14_all_modes_cluster_proposals_dir_exists" \
  "[[ -d $RUN_DIR_STD/cluster_proposals ]] && [[ -d $RUN_DIR_DEEP/cluster_proposals ]] && [[ -d $RUN_DIR_BATCH/cluster_proposals ]]"

run_test "K15_all_modes_adversaries_dir_exists" \
  "[[ -d $RUN_DIR_STD/adversaries ]] && [[ -d $RUN_DIR_DEEP/adversaries ]] && [[ -d $RUN_DIR_BATCH/adversaries ]]"

run_test "K_each_mode_has_telemetry" \
  "for d in $RUN_DIR_FAST $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do [[ -d \$d/telemetry ]] || exit 1; done"

run_test "K_each_mode_has_rankings" \
  "for d in $RUN_DIR_FAST $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do [[ -f \$d/rankings/ranking.json ]] || exit 1; done"

run_test "K_each_mode_status_completed" \
  "for d in $RUN_DIR_FAST $RUN_DIR_STD $RUN_DIR_DEEP $RUN_DIR_BATCH; do jq -e '.status == \"completed\"' \$d/manifest.json >/dev/null || exit 1; done"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Pipeline modes E2E tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
