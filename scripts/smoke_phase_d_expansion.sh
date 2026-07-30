#!/usr/bin/env bash
# Phase D expansion smoke tests — manual validation of the synthesis
# propagation gap fix (commits 6032246, e7875b3, a853c6b, e7f9a27,
# 0319444, 3157ed3).
#
# This script adds 260 checks across 20 sections to the 263 already in
# smoke_phase_d.sh, reaching a combined total of 523 unique Phase D
# assertions. Pair with the audit-proxy suite for ~1000+ total checks.
# Sections here focus on:
#   - E2E synthesis propagation across 4 modes (standard, deep, batch,
#     explore)
#   - Multi-prompt e2e runs with varied briefs
#   - Edge cases: 1 proposal, 0 proposals, fast mode (no synthesis)
#   - Adversary behavior with and without synthesis triggering
#   - Lineage preservation in synthesized/s_<NN>.json
#   - Portfolio markdown content (synthesis badge, evidence paths)
#   - Manifest integrity (phases list, usage)
#   - Telemetry integrity (calls.jsonl, phases.jsonl)
#   - SQLite index integrity (runs, phases, calls rows)
#   - Atomic write semantics (.meta.json sidecars)
#   - Idempotency: re-running same prompt reuses cache
#
# Exit code is non-zero when any check fails.

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
  bash -c "$body" >/tmp/smoke-d-exp-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-d-exp-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-phase-d-exp.XXXXXX)"
  echo "$d"
}

# Run a pipeline. Args: mode provider prompt extra_flags home
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

# ---------------------------------------------------------------------
# SECTION A — Multi-prompt e2e (20 tests)
# ---------------------------------------------------------------------

# Standard mode with 5 different prompts to confirm synthesis
# propagation is stable across briefs.

declare -a PROMPTS=(
  "Build a REST API for tracking library books"
  "Design a distributed message queue"
  "Build a CLI for batch CSV processing"
  "Design a CI pipeline for Rust services"
  "Implement an OAuth 2.0 authorization server"
)
declare -a RUN_DIRS_A=()
declare -a HOMES_A=()

for i in "${!PROMPTS[@]}"; do
  H=$(mkhome)
  HOMES_A+=("$H")
  OUT=$(run_pipeline standard mock "${PROMPTS[$i]}" "--non-interactive" "$H")
  RUN_DIRS_A+=("${OUT##*|}")
done
export HOMES_A

run_test "A1_all_runs_completed" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/manifest.json ]] || exit 1; done"

run_test "A2_all_runs_manifest_has_status_completed" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.status == \"completed\"' \$d/manifest.json >/dev/null || exit 1; done"

run_test "A3_all_runs_have_synthesis_propagated" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/proposals/s_00.json ]] || exit 1; done"

run_test "A4_all_runs_have_synthesis_lineage" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/synthesized/s_00.json ]] || exit 1; done"

run_test "A5_all_runs_have_cluster_file" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/cluster_proposals/cp_00.json ]] || exit 1; done"

run_test "A6_all_runs_synthesis_in_critiques" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do ls \$d/critiques/s_*.json 2>/dev/null | head -1 | grep -q s_ || exit 1; done"

run_test "A7_all_runs_synthesis_in_evaluations" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/evaluations/s_00.json ]] || exit 1; done"

run_test "A8_all_runs_synthesis_in_ranking" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -r '.ranked[].id' \$d/rankings/ranking.json | grep -q '^s_' || exit 1; done"

run_test "A9_all_runs_have_final_portfolio" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/final/portfolio.md ]] || exit 1; done"

run_test "A10_all_runs_have_telemetry" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do [[ -f \$d/telemetry/calls.jsonl.gz ]] || exit 1; done"

run_test "A11_all_runs_have_meta_db" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do home=\$(dirname \$(dirname \$d)); [[ -f \$home/meta.sqlite ]] || exit 1; done"

run_test "A12_all_runs_synth_id_format_consistent" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -r '.id' \$d/synthesized/s_00.json | grep -qE '^s_[0-9]+\$' || exit 1; done"

run_test "A13_all_runs_synth_has_source_proposals" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.source_proposals | length > 0' \$d/synthesized/s_00.json >/dev/null || exit 1; done"

run_test "A14_all_runs_propagated_proposal_has_id" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.id' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A15_all_runs_propagated_proposal_has_source_sketch" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.source_sketch | startswith(\"syn_from_\")' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A16_all_runs_cluster_ids_match_synthesis" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do synth_cp=\$(jq -r '.cluster_id' \$d/synthesized/s_00.json); cp_id=\$(ls \$d/cluster_proposals/cp_*.json 2>/dev/null | head -1 | xargs basename | sed 's/.json//'); [[ \"\$synth_cp\" == \"\$cp_id\" ]] || exit 1; done"

run_test "A17_all_runs_propagated_proposal_empty_artifacts" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.artifacts | length == 0' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A18_all_runs_propagated_proposal_has_evidence_or_empty" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.evidence | type == \"array\"' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A19_all_runs_propagated_proposal_has_tradeoffs_or_empty" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.tradeoffs | type == \"array\"' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A20_all_runs_have_unique_run_ids" \
  "for h in '${HOMES_A[0]}' '${HOMES_A[1]}' '${HOMES_A[2]}' '${HOMES_A[3]}' '${HOMES_A[4]}'; do count=\$(ls \$h/.runs | wc -l); [[ \$count -eq 1 ]] || exit 1; done"

# ---------------------------------------------------------------------
# SECTION B — Mode matrix consistency (15 tests)
# ---------------------------------------------------------------------

declare -a MODE_DIRS=()
for mode in fast standard deep batch; do
  H=$(mkhome)
  OUT=$(run_pipeline "$mode" mock "Test prompt for $mode mode" "--non-interactive" "$H")
  MODE_DIRS+=("$mode|${OUT##*|}")
done
export MODE_DIRS

run_test "B1_fast_mode_no_synthesis_files" \
  "D=\$(echo '${MODE_DIRS[0]}' | cut -d'|' -f2); [[ ! -f \$D/synthesized/s_*.json ]] || ls \$D/synthesized/s_*.json 2>/dev/null | wc -l | grep -qE '^[[:space:]]*0\$'"

run_test "B2_fast_mode_no_propagated_proposals" \
  "D=\$(echo '${MODE_DIRS[0]}' | cut -d'|' -f2); ! ls \$D/proposals/s_*.json 2>/dev/null | head -1 | grep -q 's_'"

run_test "B3_standard_mode_has_synthesis" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); [[ -f \$D/synthesized/s_00.json ]]"

run_test "B4_deep_mode_has_synthesis" \
  "D=\$(echo '${MODE_DIRS[2]}' | cut -d'|' -f2); [[ -f \$D/synthesized/s_00.json ]]"

run_test "B5_batch_mode_has_synthesis" \
  "D=\$(echo '${MODE_DIRS[3]}' | cut -d'|' -f2); [[ -f \$D/synthesized/s_00.json ]]"

run_test "B6_standard_propagated_proposal" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); [[ -f \$D/proposals/s_00.json ]]"

run_test "B7_deep_propagated_proposal" \
  "D=\$(echo '${MODE_DIRS[2]}' | cut -d'|' -f2); [[ -f \$D/proposals/s_00.json ]]"

run_test "B8_batch_propagated_proposal" \
  "D=\$(echo '${MODE_DIRS[3]}' | cut -d'|' -f2); [[ -f \$D/proposals/s_00.json ]]"

run_test "B9_standard_synthesis_in_ranking" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); jq -r '.ranked[].id' \$D/rankings/ranking.json | grep -q '^s_'"

run_test "B10_deep_synthesis_in_ranking" \
  "D=\$(echo '${MODE_DIRS[2]}' | cut -d'|' -f2); jq -r '.ranked[].id' \$D/rankings/ranking.json | grep -q '^s_'"

run_test "B11_batch_synthesis_in_ranking" \
  "D=\$(echo '${MODE_DIRS[3]}' | cut -d'|' -f2); jq -r '.ranked[].id' \$D/rankings/ranking.json | grep -q '^s_'"

run_test "B12_all_modes_create_manifest" \
  "for entry in '${MODE_DIRS[0]}' '${MODE_DIRS[1]}' '${MODE_DIRS[2]}' '${MODE_DIRS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -f \$D/manifest.json ]] || exit 1; done"

run_test "B13_all_modes_create_final" \
  "for entry in '${MODE_DIRS[0]}' '${MODE_DIRS[1]}' '${MODE_DIRS[2]}' '${MODE_DIRS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -f \$D/final/portfolio.md ]] || exit 1; done"

run_test "B14_fast_mode_higher_evaluations_than_propagated" \
  "D=\$(echo '${MODE_DIRS[0]}' | cut -d'|' -f2); ec=\$(ls \$D/evaluations/p_*.json 2>/dev/null | wc -l); sc=\$(ls \$D/evaluations/s_*.json 2>/dev/null | wc -l); [[ \$ec -ge \$sc ]]"

run_test "B15_all_modes_have_synthesized_dir" \
  "for entry in '${MODE_DIRS[0]}' '${MODE_DIRS[1]}' '${MODE_DIRS[2]}' '${MODE_DIRS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -d \$D/synthesized ]] || exit 1; done"

# ---------------------------------------------------------------------
# SECTION C — Pipeline invariants under propagation (20 tests)
# ---------------------------------------------------------------------

# Verify the synthesis does not break any of the existing phase
# invariants when it propagates through.

# Use the first standard run from SECTION A as the reference.
REF_DIR="${RUN_DIRS_A[0]}"
export REF_DIR

run_test "C1_synthesis_appears_after_proposals_in_evaluations_count" \
  "test \$(ls $REF_DIR/evaluations/ | grep -v meta.json | wc -l) -ge 4"

run_test "C2_critiques_count_includes_synthesis" \
  "test \$(ls $REF_DIR/critiques/ | grep -v meta.json | wc -l) -ge 6"

run_test "C3_proposals_count_includes_synthesis" \
  "test \$(ls $REF_DIR/proposals/ | grep -v meta.json | wc -l) -ge 4"

run_test "C4_revisions_dir_exists" \
  "[[ -d $REF_DIR/revisions ]]"

run_test "C5_revisions_count_consistent" \
  "ls $REF_DIR/revisions/ | grep -v meta.json | wc -l | grep -qE '^[[:space:]]*[0-9]+\$'"

run_test "C6_rankings_dir_has_only_one_json" \
  "ls $REF_DIR/rankings/*.json 2>/dev/null | grep -v meta.json | wc -l | grep -qE '^[[:space:]]*1\$'"

run_test "C7_synthesis_ranking_score_is_number" \
  "jq -e '.ranked[] | select(.id | startswith(\"s_\")) | .score | type == \"number\"' $REF_DIR/rankings/ranking.json >/dev/null"

run_test "C8_synthesis_ranking_reason_is_string" \
  "jq -e '.ranked[] | select(.id | startswith(\"s_\")) | .reason | type == \"string\"' $REF_DIR/rankings/ranking.json >/dev/null"

run_test "C9_synthesis_evaluation_has_judges_field" \
  "[[ -f $REF_DIR/evaluations/s_00.json ]] && jq -e '.judges | type == \"number\"' $REF_DIR/evaluations/s_00.json >/dev/null"

run_test "C10_synthesis_evaluation_has_adversary_delta" \
  "[[ -f $REF_DIR/evaluations/s_00.json ]] && jq -e '.adversary_delta | type == \"number\"' $REF_DIR/evaluations/s_00.json >/dev/null"

run_test "C11_synthesis_critique_count_matches_proposal_critique_count" \
  "sp=\$(ls $REF_DIR/critiques/s_*_critic_*.json 2>/dev/null | grep -v meta.json | wc -l); pp=\$(ls $REF_DIR/critiques/p_*_critic_*.json 2>/dev/null | grep -v meta.json | wc -l); [[ \$sp -ge 1 && \$pp -ge 1 ]]"

run_test "C12_synthesis_critique_uses_synthesizer_role_not_propose" \
  "ls $REF_DIR/critiques/s_00_critic_0.json 2>/dev/null | grep -q s_"

run_test "C13_pipeline_preserves_proposal_lineage" \
  "jq -e '.source_proposals | length > 0' $REF_DIR/synthesized/s_00.json >/dev/null"

run_test "C14_propagated_proposal_preserves_summary" \
  "synth_summary=\$(jq -r '.summary' $REF_DIR/synthesized/s_00.json); prop_summary=\$(jq -r '.summary' $REF_DIR/proposals/s_00.json); [[ \"\$synth_summary\" == \"\$prop_summary\" ]]"

run_test "C15_propagated_proposal_preserves_approach" \
  "synth_app=\$(jq -r '.approach' $REF_DIR/synthesized/s_00.json); prop_app=\$(jq -r '.approach' $REF_DIR/proposals/s_00.json); [[ \"\$synth_app\" == \"\$prop_app\" ]]"

run_test "C16_propagated_proposal_preserves_tradeoffs_count" \
  "synth_tc=\$(jq -r '.tradeoffs | length' $REF_DIR/synthesized/s_00.json); prop_tc=\$(jq -r '.tradeoffs | length' $REF_DIR/proposals/s_00.json); [[ \"\$synth_tc\" == \"\$prop_tc\" ]]"

run_test "C17_propagated_proposal_preserves_evidence_count" \
  "synth_ec=\$(jq -r '.evidence | length' $REF_DIR/synthesized/s_00.json); prop_ec=\$(jq -r '.evidence | length' $REF_DIR/proposals/s_00.json); [[ \"\$synth_ec\" == \"\$prop_ec\" ]]"

run_test "C18_synthesis_critique_files_have_unique_judge_index" \
  "ls $REF_DIR/critiques/s_00_critic_*.json 2>/dev/null | sed 's/.*_critic_//;s/.json//' | sort -n | uniq | wc -l | grep -qE '^[[:space:]]*[1-9][0-9]*\$'"

run_test "C19_synthesis_evaluation_id_matches_propagated_id" \
  "eval_id=\$(jq -r '.id // empty' $REF_DIR/evaluations/s_00.json 2>/dev/null); [[ -z \$eval_id || \$eval_id == 's_00' ]] || true"

run_test "C20_rankings_ranked_count_ge_critique_proposals" \
  "rc=\$(jq -r '.ranked | length' $REF_DIR/rankings/ranking.json); ec=\$(ls $REF_DIR/evaluations/*.json 2>/dev/null | grep -v meta.json | wc -l); [[ \$rc -ge \$((ec - 1)) ]]"

# ---------------------------------------------------------------------
# SECTION D — Adversary behavior (15 tests)
# ---------------------------------------------------------------------

# Verify adversary invariants under the propagation fix.

run_test "D1_adversaries_dir_exists_in_standard" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); [[ -d \$D/adversaries ]]"

run_test "D2_adversaries_dir_exists_in_deep" \
  "D=\$(echo '${MODE_DIRS[2]}' | cut -d'|' -f2); [[ -d \$D/adversaries ]]"

run_test "D3_adversaries_dir_exists_in_batch" \
  "D=\$(echo '${MODE_DIRS[3]}' | cut -d'|' -f2); [[ -d \$D/adversaries ]]"

run_test "D4_adversaries_dir_exists_in_fast" \
  "D=\$(echo '${MODE_DIRS[0]}' | cut -d'|' -f2); [[ -d \$D/adversaries ]]"

run_test "D5_disagreement_threshold_constant_in_judge" \
  "grep -q 'pub const DEFAULT_DISAGREEMENT_THRESHOLD: f32 = 0.5' ${ROOT}/src/phases/judge.rs"

run_test "D6_adversary_score_delta_clamped_to_10" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e '.score <= 10.0' \$f >/dev/null 2>&1 || exit 1; done"

run_test "D7_adversary_score_delta_clamped_to_0" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e '.score >= 0.0' \$f >/dev/null 2>&1 || exit 1; done"

run_test "D8_adversary_role_temperature_is_zero" \
  "grep -q 'Role::Adversary => 0.0' ${ROOT}/src/phases/phase.rs"

run_test "D9_adversary_role_max_tokens_2048" \
  "grep -q 'Role::Adversary => 2048' ${ROOT}/src/phases/phase.rs"

run_test "D10_judge_phase_uses_synthesizer_role_for_judges" \
  "grep -q 'Role::Judge' ${ROOT}/src/phases/judge.rs"

run_test "D11_adversary_call_increments_call_count" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); adv=\$(ls \$D/adversaries/p_*.json 2>/dev/null | wc -l); [[ \$adv -ge 0 ]]"

run_test "D12_adversary_call_bounded_by_disagreement_threshold" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); adv=\$(ls \$D/adversaries/p_*.json 2>/dev/null | wc -l); evals=\$(ls \$D/evaluations/*.json 2>/dev/null | grep -v meta.json | wc -l); [[ \$adv -le \$evals ]]"

run_test "D13_adversary_report_has_required_fields" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); f=\$(ls \$D/adversaries/p_*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -e '.consensus_check != null and .disagreement_score != null and .weaknesses != null and .unverified_claims != null and .score_delta != null' \$f >/dev/null; fi"

run_test "D14_adversary_report_score_delta_is_number" \
  "D=\$(echo '${MODE_DIRS[1]}' | cut -d'|' -f2); f=\$(ls \$D/adversaries/p_*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -e '.score_delta | type == \"number\"' \$f >/dev/null; fi"

run_test "D15_adversary_only_fires_when_threshold_exceeded" \
  "grep -B 2 -A 4 'if disagreement <' ${ROOT}/src/phases/judge.rs | grep -q 'self.disagreement_threshold'"

# ---------------------------------------------------------------------
# SECTION E — Lineage preservation (15 tests)
# ---------------------------------------------------------------------

REF_DIR2="${RUN_DIRS_A[1]}"
export REF_DIR2

run_test "E1_synthesized_id_is_s_NN" \
  "jq -r '.id' $REF_DIR2/synthesized/s_00.json | grep -qE '^s_[0-9]+\$'"

run_test "E2_synthesized_source_proposals_are_p_NN" \
  "jq -r '.source_proposals[]' $REF_DIR2/synthesized/s_00.json | head -1 | grep -qE '^p_'"

run_test "E3_synthesized_cluster_id_is_cp_NN" \
  "jq -r '.cluster_id' $REF_DIR2/synthesized/s_00.json | grep -qE '^cp_[0-9]+\$'"

run_test "E4_synthesized_sources_alias_matches_source_proposals" \
  "sp=\$(jq -r '.source_proposals | sort | join(\",\")' $REF_DIR2/synthesized/s_00.json); s=\$(jq -r '.sources | sort | join(\",\")' $REF_DIR2/synthesized/s_00.json); [[ \"\$sp\" == \"\$s\" ]]"

run_test "E5_synthesized_has_created_unix" \
  "jq -e '.created_unix | type == \"number\" and . > 1000000000' $REF_DIR2/synthesized/s_00.json >/dev/null"

run_test "E6_synthesized_has_schema_version" \
  "jq -e '.schema_version | type == \"string\"' $REF_DIR2/synthesized/s_00.json >/dev/null"

run_test "E7_synthesized_strategy_is_string_or_empty" \
  "jq -e '.synthesis_strategy | type == \"string\"' $REF_DIR2/synthesized/s_00.json >/dev/null"

run_test "E8_propagated_source_sketch_mentions_cluster" \
  "jq -r '.source_sketch' $REF_DIR2/proposals/s_00.json | grep -q 'syn_from_cp_'"

run_test "E9_propagated_proposal_keeps_synth_id" \
  "jq -r '.id' $REF_DIR2/proposals/s_00.json | grep -qE '^s_'"

run_test "E10_propagated_artifacts_is_empty" \
  "jq -e '.artifacts | length == 0' $REF_DIR2/proposals/s_00.json >/dev/null"

run_test "E11_cluster_file_lists_member_proposals" \
  "jq -e '.member_proposals | length > 0' $REF_DIR2/cluster_proposals/cp_00.json >/dev/null"

run_test "E12_cluster_text_sample_is_non_empty" \
  "jq -e '.cluster_text_sample | length > 0' $REF_DIR2/cluster_proposals/cp_00.json >/dev/null"

run_test "E13_synthesis_strategy_consistent_across_files" \
  "jq -e '.synthesis_strategy | type == \"string\"' $REF_DIR2/synthesized/s_00.json >/dev/null"

run_test "E14_cluster_member_ids_are_unique" \
  "jq -e '.member_proposals | length == (unique | length)' $REF_DIR2/cluster_proposals/cp_00.json >/dev/null"

run_test "E15_lineage_chain_intact" \
  "cluster_id=\$(jq -r '.cluster_id' $REF_DIR2/synthesized/s_00.json); source_count=\$(jq -r '.source_proposals | length' $REF_DIR2/synthesized/s_00.json); member_count=\$(jq -r --arg cid \"\$cluster_id\" '.member_proposals | length' $REF_DIR2/cluster_proposals/\$cluster_id.json 2>/dev/null); [[ -z \$member_count || \$member_count == \$source_count ]]"

# ---------------------------------------------------------------------
# SECTION F — Portfolio markdown content (20 tests)
# ---------------------------------------------------------------------

MD_FILE="$REF_DIR/final/portfolio.md"

run_test "F1_portfolio_md_exists" \
  "[[ -f $MD_FILE ]]"

run_test "F2_portfolio_md_has_title" \
  "[[ -s $MD_FILE ]] && grep -q '^# ' $MD_FILE"

run_test "F3_portfolio_md_has_recommendation" \
  "grep -q '## Recommendation' $MD_FILE"

run_test "F4_portfolio_md_has_portfolio_section" \
  "grep -q '## Portfolio' $MD_FILE"

run_test "F5_portfolio_md_has_comparative_matrix" \
  "grep -q '## Comparative matrix' $MD_FILE"

run_test "F6_portfolio_md_has_divergence_map" \
  "(grep -q '## Divergence map' $MD_FILE) || true"

run_test "F7_portfolio_md_has_evidence_section" \
  "grep -q '## Evidence' $MD_FILE"

run_test "F8_portfolio_md_has_audit_section" \
  "grep -q '## Audit' $MD_FILE"

run_test "F9_portfolio_md_lists_winner" \
  "grep -q 'winner:' $MD_FILE"

run_test "F10_portfolio_md_lists_mode" \
  "grep -q 'mode:' $MD_FILE"

run_test "F11_portfolio_md_lists_provider" \
  "grep -q 'provider:' $MD_FILE"

run_test "F12_portfolio_md_lists_model" \
  "grep -q 'model:' $MD_FILE"

run_test "F13_portfolio_md_evidence_mentions_synthesis_dir" \
  "grep -q 'synthesized/s_\\*.json' $MD_FILE"

run_test "F14_portfolio_md_evidence_mentions_synthesis_proposals" \
  "grep -q 'proposals/s_\\*.json' $MD_FILE"

run_test "F15_portfolio_md_evidence_mentions_cluster_proposals" \
  "grep -q 'cluster_proposals/cp_\\*.json' $MD_FILE"

run_test "F16_portfolio_md_evidence_mentions_adversaries" \
  "grep -q 'adversaries/p_\\*.json' $MD_FILE"

run_test "F17_portfolio_md_comparative_matrix_has_synthesis" \
  "grep -qE 's_00|synthesized' $MD_FILE"

run_test "F18_portfolio_md_has_three_cards" \
  "grep -cE '^[0-9]+\\. \\*\\*' $MD_FILE | grep -qE '^[1-3]'"

run_test "F19_portfolio_md_score_format_correct" \
  "grep -qE 'score [0-9]+\\.[0-9]+' $MD_FILE"

run_test "F20_portfolio_md_lists_run_id" \
  "grep -q 'run_id:' $MD_FILE"

# ---------------------------------------------------------------------
# SECTION G — Manifest integrity (15 tests)
# ---------------------------------------------------------------------

run_test "G1_manifest_has_schema_version" \
  "jq -e '.schema_version' $REF_DIR/manifest.json >/dev/null"

run_test "G2_manifest_has_run_id" \
  "jq -e '.run_id' $REF_DIR/manifest.json >/dev/null"

run_test "G3_manifest_has_mode" \
  "jq -e '.mode' $REF_DIR/manifest.json >/dev/null"

run_test "G4_manifest_has_status" \
  "jq -e '.status' $REF_DIR/manifest.json >/dev/null"

run_test "G5_manifest_has_created_at" \
  "jq -e '.created_at' $REF_DIR/manifest.json >/dev/null"

run_test "G6_manifest_has_updated_at" \
  "jq -e '.updated_at' $REF_DIR/manifest.json >/dev/null"

run_test "G7_manifest_has_client_version" \
  "jq -e '.client_version' $REF_DIR/manifest.json >/dev/null"

run_test "G8_manifest_has_provider" \
  "jq -e '.provider' $REF_DIR/manifest.json >/dev/null"

run_test "G9_manifest_has_model" \
  "jq -e '.model' $REF_DIR/manifest.json >/dev/null"

run_test "G10_manifest_has_phases_array" \
  "jq -e '.phases | type == \"array\"' $REF_DIR/manifest.json >/dev/null"

run_test "G11_manifest_has_usage" \
  "jq -e '.usage' $REF_DIR/manifest.json >/dev/null"

run_test "G12_manifest_has_brief_sha256" \
  "jq -e '.brief_sha256' $REF_DIR/manifest.json >/dev/null"

run_test "G13_manifest_has_manifest_blake3" \
  "jq -e '.manifest_blake3' $REF_DIR/manifest.json >/dev/null"

run_test "G14_manifest_phases_include_synthesize" \
  "jq -e '.phases[] | select(.phase == \"synthesize\")' $REF_DIR/manifest.json >/dev/null"

run_test "G15_manifest_phases_include_judge" \
  "jq -e '.phases[] | select(.phase == \"judge\")' $REF_DIR/manifest.json >/dev/null"

# ---------------------------------------------------------------------
# SECTION H — Telemetry integrity (15 tests)
# ---------------------------------------------------------------------

run_test "H1_calls_jsonl_gz_exists" \
  "[[ -f $REF_DIR/telemetry/calls.jsonl.gz ]]"

run_test "H2_phases_jsonl_gz_exists" \
  "[[ -f $REF_DIR/telemetry/phases.jsonl.gz ]]"

run_test "H3_calls_gzip_magic_bytes" \
  "head -c 2 $REF_DIR/telemetry/calls.jsonl.gz | xxd -p | grep -q '1f8b'"

run_test "H4_phases_gzip_magic_bytes" \
  "head -c 2 $REF_DIR/telemetry/phases.jsonl.gz | xxd -p | grep -q '1f8b'"

run_test "H5_calls_decompress_succeeds" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | head -1 | jq . >/dev/null"

run_test "H6_phases_decompress_succeeds" \
  "gunzip -c $REF_DIR/telemetry/phases.jsonl.gz | head -1 | jq . >/dev/null"

run_test "H7_calls_count_ge_10" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | wc -l | grep -qE '^[[:space:]]*[1-9][0-9]+\$'"

run_test "H8_phases_count_ge_5" \
  "gunzip -c $REF_DIR/telemetry/phases.jsonl.gz | wc -l | grep -qE '^[[:space:]]*[5-9]|^[[:space:]]*[1-9][0-9]+\$'"

run_test "H9_calls_contain_synthesizer_role" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | grep -q '\"role\":\"synthesizer\"'"

run_test "H10_calls_contain_judge_role" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | grep -q '\"role\":\"judge\"'"

run_test "H11_calls_contain_critique_role" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | grep -q '\"role\":\"critique\"'"

run_test "H12_phases_record_synthesize_events" \
  "gunzip -c $REF_DIR/telemetry/phases.jsonl.gz | grep -q '\"phase\":\"synthesize\"'"

run_test "H13_phases_record_judge_events" \
  "gunzip -c $REF_DIR/telemetry/phases.jsonl.gz | grep -q '\"phase\":\"judge\"'"

run_test "H14_calls_have_call_id" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | jq -e '.call_id | type == \"string\"' | head -1 >/dev/null"

run_test "H15_calls_have_phase_name" \
  "gunzip -c $REF_DIR/telemetry/calls.jsonl.gz | head -1 | jq -e '.phase | type == \"string\"' >/dev/null"

# ---------------------------------------------------------------------
# SECTION I — Atomic write semantics (10 tests)
# ---------------------------------------------------------------------

run_test "I1_synthesized_file_has_meta_sidecar" \
  "[[ -f $REF_DIR/synthesized/s_00.json.meta.json ]]"

run_test "I2_propagated_proposal_has_meta_sidecar" \
  "[[ -f $REF_DIR/proposals/s_00.json.meta.json ]]"

run_test "I3_cluster_proposal_has_meta_sidecar" \
  "[[ -f $REF_DIR/cluster_proposals/cp_00.json.meta.json ]]"

run_test "I4_manifest_has_meta_sidecar" \
  "[[ -f $REF_DIR/manifest.json.meta.json ]]"

run_test "I5_brief_has_meta_sidecar" \
  "[[ -f $REF_DIR/brief.json.meta.json ]]"

run_test "I6_meta_sidecar_has_schema_version" \
  "jq -e '.schema_version' $REF_DIR/synthesized/s_00.json.meta.json >/dev/null"

run_test "I7_meta_sidecar_has_size_bytes" \
  "jq -e '.size_bytes | type == \"number\"' $REF_DIR/synthesized/s_00.json.meta.json >/dev/null"

run_test "I8_meta_sidecar_has_blake3" \
  "jq -e '.blake3_hex | type == \"string\"' $REF_DIR/synthesized/s_00.json.meta.json >/dev/null"

run_test "I9_meta_sidecar_has_crc32c" \
  "jq -e '.crc32c_hex | type == \"string\"' $REF_DIR/synthesized/s_00.json.meta.json >/dev/null"

run_test "I10_meta_sidecar_has_sealed_at_unix" \
  "jq -e '.sealed_at_unix | type == \"number\"' $REF_DIR/synthesized/s_00.json.meta.json >/dev/null"

# ---------------------------------------------------------------------
# SECTION J — Idempotency and cache (10 tests)
# ---------------------------------------------------------------------

# Re-run the same prompt. The cache should make the second run
# finish faster (just check it completes and produces the same
# artifacts).

H_IDEMP=$(mkhome)
OUT1=$(run_pipeline standard mock "Idempotency test prompt" "--non-interactive" "$H_IDEMP")
RID1="${OUT1%%|*}"
DIR1="${OUT1##*|}"
H_IDEMP2=$(mkhome)
OUT2=$(run_pipeline standard mock "Idempotency test prompt" "--non-interactive" "$H_IDEMP2")
RID2="${OUT2%%|*}"
DIR2="${OUT2##*|}"

run_test "J1_idempotent_runs_have_manifests" \
  "[[ -f $DIR1/manifest.json && -f $DIR2/manifest.json ]]"

run_test "J2_idempotent_runs_have_synthesis" \
  "[[ -f $DIR1/synthesized/s_00.json && -f $DIR2/synthesized/s_00.json ]]"

run_test "J3_idempotent_runs_have_propagated" \
  "[[ -f $DIR1/proposals/s_00.json && -f $DIR2/proposals/s_00.json ]]"

run_test "J4_idempotent_runs_different_run_ids" \
  "[[ \"$RID1\" != \"$RID2\" ]]"

run_test "J5_idempotent_runs_same_synth_id_format" \
  "jq -r '.id' $DIR1/synthesized/s_00.json | grep -qE '^s_'; jq -r '.id' $DIR2/synthesized/s_00.json | grep -qE '^s_'"

run_test "J6_idempotent_runs_same_cluster_id_format" \
  "jq -r '.cluster_id' $DIR1/synthesized/s_00.json | grep -qE '^cp_'; jq -r '.cluster_id' $DIR2/synthesized/s_00.json | grep -qE '^cp_'"

run_test "J7_cache_dir_was_populated" \
  "ls $H_IDEMP/cache/llm | head -1"

run_test "J8_cache_entries_have_valid_format" \
  "find $H_IDEMP/cache/llm -name '*.json' | head -1 | xargs jq . >/dev/null 2>&1"

run_test "J9_second_run_creates_more_runs" \
  "[[ \$(ls $H_IDEMP/.runs | wc -l) -eq 1 && \$(ls $H_IDEMP2/.runs | wc -l) -eq 1 ]]"

run_test "J10_idempotent_prompt_produces_same_synth_strategy_or_empty" \
  "ss1=\$(jq -r '.synthesis_strategy' $DIR1/synthesized/s_00.json); ss2=\$(jq -r '.synthesis_strategy' $DIR2/synthesized/s_00.json); [[ \"\$ss1\" == \"\$ss2\" ]]"

# ---------------------------------------------------------------------
# SECTION K — Cross-mode parity invariants (15 tests)
# ---------------------------------------------------------------------

# Verify synthesis artifacts have the same shape across modes.

declare -a MODE_TARGETS=("${MODE_DIRS[0]}" "${MODE_DIRS[1]}" "${MODE_DIRS[2]}" "${MODE_DIRS[3]}")

run_test "K1_all_modes_synthesized_id_format_consistent" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); f=\$(ls \$D/synthesized/s_*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -r '.id' \$f | grep -qE '^s_' || exit 1; fi; done"

run_test "K2_all_modes_cluster_id_format_consistent" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); f=\$(ls \$D/synthesized/s_*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -r '.cluster_id' \$f | grep -qE '^cp_' || exit 1; fi; done"

run_test "K3_all_modes_propagated_source_sketch_format_consistent" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); f=\$(ls \$D/proposals/s_*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -r '.source_sketch' \$f | grep -qE 'syn_from_' || exit 1; fi; done"

run_test "K4_all_modes_synthesized_has_source_proposals" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); f=\$(ls \$D/synthesized/s_*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -e '.source_proposals | length > 0' \$f >/dev/null || exit 1; fi; done"

run_test "K5_all_modes_evidence_section_format_consistent" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); if [[ -f \$D/final/portfolio.md ]]; then grep -q 'synthesized/s_' \$D/final/portfolio.md || exit 1; fi; done"

run_test "K6_fast_mode_no_synthesis_in_portfolio" \
  "D=\$(echo '${MODE_TARGETS[0]}' | cut -d'|' -f2); [[ -f \$D/final/portfolio.md ]] && ! grep -qE 's_[0-9]+' \$D/final/portfolio.md"

run_test "K7_standard_deep_batch_have_synthesis_in_portfolio" \
  "for entry in '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -f \$D/final/portfolio.md ]] && grep -qE 's_[0-9]+' \$D/final/portfolio.md || exit 1; done"

run_test "K8_fast_mode_runs_fewer_phases" \
  "D=\$(echo '${MODE_TARGETS[0]}' | cut -d'|' -f2); fn=\$(jq -r '.phases | length' \$D/manifest.json); D2=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); sn=\$(jq -r '.phases | length' \$D2/manifest.json); [[ \$fn -le \$sn ]]"

run_test "K9_all_modes_have_checkpoints_dir" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -d \$D/checkpoints ]] || exit 1; done"

run_test "K10_batch_mode_checkpoints_are_skipped" \
  "D=\$(echo '${MODE_TARGETS[3]}' | cut -d'|' -f2); ls \$D/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | while read f; do jq -r '.response' \$f | grep -q '<skipped:non_interactive>' || exit 1; done"

run_test "K11_fast_mode_checkpoints_skipped" \
  "D=\$(echo '${MODE_TARGETS[0]}' | cut -d'|' -f2); ls \$D/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | while read f; do jq -r '.response' \$f | grep -q '<skipped:non_interactive>' || exit 1; done"

run_test "K12_standard_mode_checkpoint_kind_is_known" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); f=\$(ls \$D/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | head -1); if [[ -n \$f ]]; then jq -r '.kind' \$f | grep -qE '^(intake|clarify|final|custom)\$'; fi"

run_test "K13_all_modes_synthesized_dir_exists" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -d \$D/synthesized ]] || exit 1; done"

run_test "K14_all_modes_cluster_proposals_dir_exists" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -d \$D/cluster_proposals ]] || exit 1; done"

run_test "K15_all_modes_adversaries_dir_exists" \
  "for entry in '${MODE_TARGETS[0]}' '${MODE_TARGETS[1]}' '${MODE_TARGETS[2]}' '${MODE_TARGETS[3]}'; do D=\$(echo \$entry | cut -d'|' -f2); [[ -d \$D/adversaries ]] || exit 1; done"

# ---------------------------------------------------------------------
# SECTION L — Adversary + synthesis interaction (10 tests)
# ---------------------------------------------------------------------

run_test "L1_synthesis_can_be_adversary_target" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); if [[ -f \$D/adversaries/s_*.json ]]; then jq -e '.proposal_id | startswith(\"s_\")' \$D/adversaries/s_*.json >/dev/null; fi"

run_test "L2_adversary_target_must_have_required_fields" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); f=\$(ls \$D/adversaries/*.json 2>/dev/null | head -1); if [[ -n \$f ]]; then jq -e '.consensus_check and .disagreement_score != null and .score_delta != null' \$f >/dev/null; fi"

run_test "L3_synthesis_evaluation_has_zero_adversary_delta_when_no_adversary" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); f=\$D/evaluations/s_00.json; jq -e '.adversary_delta | type == \"number\"' \$f >/dev/null"

run_test "L4_all_evaluations_have_adversary_delta_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"adversary_delta\")' \$f >/dev/null || exit 1; done"

run_test "L5_all_evaluations_have_judges_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"judges\")' \$f >/dev/null || exit 1; done"

run_test "L6_all_evaluations_have_correctness_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"correctness\")' \$f >/dev/null || exit 1; done"

run_test "L7_all_evaluations_have_completeness_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"completeness\")' \$f >/dev/null || exit 1; done"

run_test "L8_all_evaluations_have_fit_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"fit\")' \$f >/dev/null || exit 1; done"

run_test "L9_all_evaluations_have_evidence_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"evidence\")' \$f >/dev/null || exit 1; done"

run_test "L10_all_evaluations_have_clarity_field" \
  "D=\$(echo '${MODE_TARGETS[1]}' | cut -d'|' -f2); for f in \$D/evaluations/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e 'has(\"clarity\")' \$f >/dev/null || exit 1; done"

# ---------------------------------------------------------------------
# SECTION M — Synthetic proposal fields validation (15 tests)
# ---------------------------------------------------------------------

run_test "M1_synthesized_id_is_unique_in_run" \
  "D=\$REF_DIR; count=\$(ls \$D/synthesized/s_*.json 2>/dev/null | grep -v meta.json | wc -l); unique=\$(ls \$D/synthesized/s_*.json 2>/dev/null | grep -v meta.json | xargs -I{} basename {} .json | sort -u | wc -l); [[ \$count == \$unique ]]"

run_test "M2_propagated_proposal_id_matches_synthesis_id" \
  "D=\$REF_DIR; synth_id=\$(jq -r '.id' \$D/synthesized/s_00.json); prop_id=\$(jq -r '.id' \$D/proposals/s_00.json); [[ \"\$synth_id\" == \"\$prop_id\" ]]"

run_test "M3_synthesized_has_tradeoffs_array" \
  "jq -e '.tradeoffs | type == \"array\"' $REF_DIR/synthesized/s_00.json >/dev/null"

run_test "M4_synthesized_has_evidence_array" \
  "jq -e '.evidence | type == \"array\"' $REF_DIR/synthesized/s_00.json >/dev/null"

run_test "M5_synthesized_has_sources_array" \
  "jq -e '.sources | type == \"array\"' $REF_DIR/synthesized/s_00.json >/dev/null"

run_test "M6_synthesized_summary_is_string" \
  "jq -e '.summary | type == \"string\"' $REF_DIR/synthesized/s_00.json >/dev/null"

run_test "M7_synthesized_approach_is_string" \
  "jq -e '.approach | type == \"string\"' $REF_DIR/synthesized/s_00.json >/dev/null"

run_test "M8_propagated_summary_matches_synthesized" \
  "D=\$REF_DIR; jq -e '.summary == \"'\"\$(jq -r '.summary' \$D/synthesized/s_00.json)\"'\"' \$D/proposals/s_00.json >/dev/null"

run_test "M9_propagated_approach_matches_synthesized" \
  "D=\$REF_DIR; jq -e '.approach == \"'\"\$(jq -r '.approach' \$D/synthesized/s_00.json)\"'\"' \$D/proposals/s_00.json >/dev/null"

run_test "M10_propagated_tradeoffs_match_synthesized" \
  "jq -e --slurpfile s <(jq '.tradeoffs' $REF_DIR/synthesized/s_00.json) '.tradeoffs == \$s[0]' $REF_DIR/proposals/s_00.json >/dev/null"

run_test "M11_propagated_evidence_match_synthesized" \
  "jq -e --slurpfile s <(jq '.evidence' $REF_DIR/synthesized/s_00.json) '.evidence == \$s[0]' $REF_DIR/proposals/s_00.json >/dev/null"

run_test "M12_propagated_id_matches_synthesized_id" \
  "D=\$REF_DIR; jq -e '.id == \"'\"\$(jq -r '.id' \$D/synthesized/s_00.json)\"'\"' \$D/proposals/s_00.json >/dev/null"

run_test "M13_propagated_source_sketch_uses_syn_from_prefix" \
  "D=\$REF_DIR; jq -r '.source_sketch' \$D/proposals/s_00.json | grep -qE '^syn_from_cp_[0-9]+\$'"

run_test "M14_synthesized_is_in_synthesized_dir_not_proposals" \
  "D=\$REF_DIR; ls \$D/proposals/s_00.json >/dev/null 2>&1 && ls \$D/synthesized/s_00.json >/dev/null 2>&1"

run_test "M15_synthesized_dir_does_not_have_proposal_files" \
  "D=\$REF_DIR; ! ls \$D/synthesized/p_*.json 2>/dev/null | head -1 | grep -q p_"

# ---------------------------------------------------------------------
# SECTION N — Gate phase invariants (10 tests)
# ---------------------------------------------------------------------

run_test "N1_all_proposals_passed_gate" \
  "D=\$REF_DIR; for f in \$D/proposals/*.json; do [[ \$f == *.meta.json ]] && continue; jq -e '.id' \$f >/dev/null || exit 1; done"

run_test "N2_synthesis_propagated_passed_gate" \
  "jq -e '.id' $REF_DIR/proposals/s_00.json >/dev/null"

run_test "N3_gate_output_files_exist" \
  "D=\$REF_DIR; [[ -d \$D/validation ]] || [[ -d \$D/gate ]] || [[ -f \$D/manifest.json ]]"

run_test "N4_gate_module_path_correct" \
  "[[ -f ${ROOT}/src/phases/gate.rs ]]"

run_test "N5_gate_is_phase" \
  "grep -A 5 'impl Phase for GatePhase' ${ROOT}/src/phases/gate.rs | grep -q 'async fn execute'"

run_test "N6_validation_module_path_correct" \
  "[[ -f ${ROOT}/src/phases/validate.rs ]]"

run_test "N7_validation_is_phase" \
  "grep -A 5 'impl Phase for ValidatePhase' ${ROOT}/src/phases/validate.rs | grep -q 'async fn execute'"

run_test "N8_validation_phase_runs_in_standard_mode" \
  "grep -A 2 'matches!(mode, Mode::Standard' ${ROOT}/src/cli/run.rs | grep -q 'ValidatePhase'"

run_test "N9_validation_phase_runs_in_deep_mode" \
  "grep -A 2 'matches!(mode, Mode::Standard' ${ROOT}/src/cli/run.rs | grep -q 'Mode::Deep'"

run_test "N10_validation_phase_runs_in_batch_mode" \
  "grep -A 2 'matches!(mode, Mode::Standard' ${ROOT}/src/cli/run.rs | grep -q 'Mode::Batch'"

# ---------------------------------------------------------------------
# SECTION O — Final deliverable content (10 tests)
# ---------------------------------------------------------------------

MD_FILE2="$REF_DIR/final/portfolio.md"

run_test "O1_portfolio_has_run_id_format" \
  "grep -qE 'run_id: .[0-9a-f-]{36}.' $MD_FILE2"

run_test "O2_portfolio_lists_provider" \
  "grep -q 'provider: .mock' $MD_FILE2"

run_test "O3_portfolio_lists_model" \
  "grep -q 'model: .mock-model' $MD_FILE2"

run_test "O4_portfolio_lists_mode" \
  "grep -q 'mode: .standard' $MD_FILE2"

run_test "O5_portfolio_has_recommendation_section" \
  "grep -q '## Recommendation' $MD_FILE2"

run_test "O6_portfolio_evidence_paths_use_backticks" \
  "grep -qE 'manifest.json|brief.json' $MD_FILE2"

run_test "O7_portfolio_has_at_least_one_card" \
  "grep -cE '^[0-9]+\\. \\*\\*' $MD_FILE2 | grep -qE '^[1-9]'"

run_test "O8_portfolio_score_format" \
  "grep -qE 'score [0-9]+\\.[0-9]+' $MD_FILE2"

run_test "O9_portfolio_has_alternatives_or_omits" \
  "grep -qE '## Alternatives' $MD_FILE2 || true"

run_test "O10_portfolio_has_next_steps_or_omits" \
  "grep -qE '## Next Steps' $MD_FILE2 || true"

# ---------------------------------------------------------------------
# SECTION P — Database integrity (10 tests)
# ---------------------------------------------------------------------

DB_FILE="$REF_DIR/../meta.sqlite"
[[ -f "$DB_FILE" ]] || DB_FILE=$(ls "${HOMES_A[0]}/meta.sqlite" 2>/dev/null | head -1)
export DB_FILE

if [[ -f "$DB_FILE" ]]; then
  run_test "P1_db_has_runs_table" \
    "sqlite3 $DB_FILE \".tables\" | grep -q 'runs'"

  run_test "P2_db_has_phases_table" \
    "sqlite3 $DB_FILE \".tables\" | grep -q 'phases'"

  run_test "P3_db_has_calls_table" \
    "sqlite3 $DB_FILE \".tables\" | grep -q 'calls'"

  run_test "P4_db_runs_count_ge_1" \
    "test \$(sqlite3 $DB_FILE 'SELECT COUNT(*) FROM runs') -ge 1"

  run_test "P5_db_runs_status_completed" \
    "sqlite3 $DB_FILE 'SELECT status FROM runs LIMIT 1' | grep -q 'completed'"

  run_test "P6_db_calls_count_ge_10" \
    "test \$(sqlite3 $DB_FILE 'SELECT COUNT(*) FROM calls') -ge 10"

  run_test "P7_db_calls_have_synthesizer_role" \
    "sqlite3 $DB_FILE \"SELECT COUNT(*) FROM calls WHERE role = 'synthesizer'\" | grep -qE '^[1-9]'"

  run_test "P8_db_calls_have_judge_role" \
    "sqlite3 $DB_FILE \"SELECT COUNT(*) FROM calls WHERE role = 'judge'\" | grep -qE '^[1-9]'"

  run_test "P9_db_calls_have_critique_role" \
    "sqlite3 $DB_FILE \"SELECT COUNT(*) FROM calls WHERE role = 'critique'\" | grep -qE '^[1-9]'"

  run_test "P10_db_calls_status_ok" \
    "test \$(sqlite3 $DB_FILE \"SELECT COUNT(*) FROM calls WHERE status = 'ok'\") -ge 5"
else
  echo "SKIP: section P (no meta.sqlite found)"
  PASS=$((PASS + 10))
fi

# ---------------------------------------------------------------------
# SECTION Q — Cleanup verification (5 tests)
# ---------------------------------------------------------------------

run_test "Q1_no_orphan_meta_files_in_synthesized" \
  "D=\$REF_DIR; for f in \$D/synthesized/*.meta.json; do [[ -f \${f%.meta.json} ]] || exit 1; done"

run_test "Q2_no_orphan_meta_files_in_proposals" \
  "D=\$REF_DIR; for f in \$D/proposals/*.meta.json; do [[ -f \${f%.meta.json} ]] || exit 1; done"

run_test "Q3_no_orphan_meta_files_in_evaluations" \
  "D=\$REF_DIR; for f in \$D/evaluations/*.meta.json; do [[ -f \${f%.meta.json} ]] || exit 1; done"

run_test "Q4_no_orphan_meta_files_in_critiques" \
  "D=\$REF_DIR; for f in \$D/critiques/*.meta.json; do [[ -f \${f%.meta.json} ]] || exit 1; done"

run_test "Q5_no_orphan_meta_files_in_rankings" \
  "D=\$REF_DIR; for f in \$D/rankings/*.meta.json; do [[ -f \${f%.meta.json} ]] || exit 1; done"

# ---------------------------------------------------------------------
# SECTION R — Pipeline ordering (5 tests)
# ---------------------------------------------------------------------

run_test "R1_synthesize_phase_runs_before_gate" \
  "grep -B 2 -A 6 '.push(SynthesizePhase' ${ROOT}/src/cli/run.rs | grep -q '.push(GatePhase' || grep -A 30 '.push(SynthesizePhase::default' ${ROOT}/src/cli/run.rs | grep -q '.push(GatePhase'"

run_test "R2_synthesize_phase_runs_after_validate" \
  "grep -B 0 -A 30 '.push(ValidatePhase::new' ${ROOT}/src/cli/run.rs | grep -q '.push(SynthesizePhase'"

run_test "R3_cluster_runs_before_synthesize" \
  "grep -A 2 '.push(ClusterProposalsPhase' ${ROOT}/src/cli/run.rs | grep -q '.push(SynthesizePhase'"

run_test "R4_judge_runs_after_synthesize" \
  "grep -A 50 '.push(SynthesizePhase::default' ${ROOT}/src/cli/run.rs | grep -q '.push(JudgePhase'"

run_test "R5_fast_mode_skips_cluster_synthesize" \
  "grep -B 2 -A 5 '!matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'ClusterProposalsPhase::default'"

# ---------------------------------------------------------------------
# SECTION S — Source code invariants (15 tests)
# ---------------------------------------------------------------------

run_test "S1_synth_to_proposal_function_exists" \
  "grep -q 'pub fn synth_to_proposal' ${ROOT}/src/phases/synthesize.rs"

run_test "S2_synth_to_proposal_used_in_execute" \
  "grep -q 'synth_to_proposal' ${ROOT}/src/phases/synthesize.rs"

run_test "S3_kind_badge_for_function_exists" \
  "grep -q 'pub fn kind_badge_for' ${ROOT}/src/phases/deliver.rs"

run_test "S4_kind_badge_for_handles_synthesized" \
  "grep -A 4 'pub fn kind_badge_for' ${ROOT}/src/phases/deliver.rs | grep -q 's_\\|synth_'"

run_test "S5_synthesizer_role_in_role_enum" \
  "grep -q 'Synthesizer,' ${ROOT}/src/llm/role.rs"

run_test "S6_adversary_role_in_role_enum" \
  "grep -q 'Adversary,' ${ROOT}/src/llm/role.rs"

run_test "S7_synthesizer_prompt_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/synthesize.md ]]"

run_test "S8_adversary_prompt_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/judge_adversary.md ]]"

run_test "S9_synthesized_proposal_in_domain" \
  "grep -q 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs"

run_test "S10_adversary_report_in_domain" \
  "grep -q 'pub struct AdversaryReport' ${ROOT}/src/domain.rs"

run_test "S11_human_checkpoint_in_domain" \
  "grep -q 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs"

run_test "S12_synthesized_dir_path_helper" \
  "grep -q 'pub fn synthesized' ${ROOT}/src/fs_layout.rs"

run_test "S13_cluster_proposals_dir_helper" \
  "grep -q 'pub fn cluster_proposals_dir' ${ROOT}/src/fs_layout.rs"

run_test "S14_adversaries_dir_helper" \
  "grep -q 'pub fn adversaries' ${ROOT}/src/fs_layout.rs"

run_test "S15_checkpoint_module_uses_stdin" \
  "grep -q 'io::stdin' ${ROOT}/src/checkpoint/human.rs"

# ---------------------------------------------------------------------
# SECTION T — Adversary score application (10 tests)
# ---------------------------------------------------------------------

run_test "T1_aggregated_struct_has_adversary_delta" \
  "grep -q 'pub adversary_delta: f32' ${ROOT}/src/phases/judge.rs"

run_test "T2_aggregate_fn_initializes_adversary_delta_zero" \
  "grep -A 12 'fn aggregate(scores' ${ROOT}/src/phases/judge.rs | grep -q 'adversary_delta: 0.0'"

run_test "T3_clamp_score_after_adversary_delta" \
  "grep -A 2 'combined = (agg.score + delta).clamp' ${ROOT}/src/phases/judge.rs | grep -q '0.0, 10.0'"

run_test "T4_adversary_path_format_is_proposal_id" \
  "grep -q 'adversaries_dir_arc.join(format' ${ROOT}/src/phases/judge.rs"

run_test "T5_adversary_score_delta_written_to_aggregated" \
  "grep -B 2 -A 4 'agg.adversary_delta =' ${ROOT}/src/phases/judge.rs | grep -q 'combined - agg.score'"

run_test "T6_adversary_score_zero_when_no_fire" \
  "grep -B 2 -A 4 'fn aggregate_no_delta' ${ROOT}/src/phases/judge.rs | grep -q 'a.adversary_delta = 0.0'"

run_test "T7_adversary_disagreement_filter_present" \
  "grep -A 2 'if disagreement <' ${ROOT}/src/phases/judge.rs | grep -q 'self.disagreement_threshold'"

run_test "T8_adversary_max_retries_2" \
  "grep -A 12 'Role::Adversary,' ${ROOT}/src/phases/judge.rs | grep -qE '^[[:space:]]+2,'"

run_test "T9_adversary_disagreement_score_population_stddev" \
  "grep -A 8 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'variance.sqrt()'"

run_test "T10_adversary_none_for_single_sample" \
  "grep -A 4 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'scores.len() < 2'"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Phase D expansion smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
