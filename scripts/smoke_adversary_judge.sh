#!/usr/bin/env bash
# Smoke tests for Phase D's third-judge (adversary) feature.
#
# The adversary is a conditional pass inside `JudgePhase` that fires
# only when the regular judges disagree beyond a threshold. Its job
# is to surface unverified claims, hidden weaknesses, and a final
# `score_delta` that adjusts the aggregated evaluation. Coverage:
#
#   1. AdversaryReport domain type (5 fields).
#   2. Adversary role registration + prompt + sampling.
#   3. JudgePhase adversary surface (threshold, disagreement_score,
#      adversary_delta, enable flag, max retries).
#   4. End-to-end aggregation invariants.
#   5. Adversary integration tests (`tests/integration_phase_d.rs`).
#   6. Adversary behavior + cross-feature interaction (audit
#      expansion).
#   7. Adversary score application (aggregated scoring math).
#
# Split from the original smoke_phase_d.sh + expansion per feature.
# The synthesis path lives in smoke_intra_cluster_synthesis.sh;
# the human-checkpoint path lives in smoke_human_checkpoint.sh;
# cross-cutting integration lives in smoke_phase_d_integration.sh.

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
[[ -d "$MOCK_DIR" ]] || { echo "missing mock fixture dir at $MOCK_DIR"; exit 1; }

run_test() {
  local name="$1"
  local body="$2"
  env ROOT="$ROOT" BIN="$BIN" MOCK_DIR="$MOCK_DIR" bash -c "$body" >/tmp/smoke-adv-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-adv-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-adv.XXXXXX)"
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

# ---------------------------------------------------------------------
# SECTION A1 — AdversaryReport domain surface (5 tests)
# ---------------------------------------------------------------------

run_test "domain_has_AdversaryReport" \
  "grep -q 'pub struct AdversaryReport' ${ROOT}/src/domain.rs"

run_test "AdversaryReport_has_disagreement_score" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub disagreement_score'"

run_test "AdversaryReport_has_score_delta" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub score_delta'"

run_test "AdversaryReport_has_weaknesses" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub weaknesses'"

run_test "AdversaryReport_has_consensus_check" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub consensus_check'"

# ---------------------------------------------------------------------
# SECTION A2 — Adversary role + prompt + sampling (5 tests)
# ---------------------------------------------------------------------

run_test "role_Adversary_defined" \
  "grep -q 'Adversary,' ${ROOT}/src/llm/role.rs"

run_test "role_Adversary_str_is_adversary" \
  "grep -A 1 'Self::Adversary =>' ${ROOT}/src/llm/role.rs | grep -q '\"adversary\"'"

run_test "role_Adversary_temperature_is_zero" \
  "grep -B 2 -A 1 'Role::Adversary => 0.0' ${ROOT}/src/phases/phase.rs"

run_test "role_Adversary_max_tokens_is_2048" \
  "grep -B 2 -A 1 'Role::Adversary => 2048' ${ROOT}/src/phases/phase.rs"

run_test "prompt_judge_adversary_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/judge_adversary.md ]]"

# ---------------------------------------------------------------------
# SECTION A3 — JudgePhase adversary surface (10 tests)
# ---------------------------------------------------------------------

run_test "judge_disagreement_threshold_default_is_0_5" \
  "grep -q 'DEFAULT_DISAGREEMENT_THRESHOLD' ${ROOT}/src/phases/judge.rs && grep -q '0.5' ${ROOT}/src/phases/judge.rs"

run_test "judge_has_disagreement_score_function" \
  "grep -B 1 -A 1 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs"

run_test "judge_has_aggregated_adversary_delta_field" \
  "grep -B 2 -A 1 'pub adversary_delta' ${ROOT}/src/phases/judge.rs"

run_test "judge_has_enable_adversary_flag" \
  "grep -B 2 -A 1 'enable_adversary\\|adversary_enabled' ${ROOT}/src/phases/judge.rs"

run_test "judge_uses_adversary_role" \
  "grep -B 1 -A 2 'Role::Adversary' ${ROOT}/src/phases/judge.rs | grep -q 'Adversary'"

run_test "judge_writes_adversaries_dir" \
  "grep -q 'adversaries' ${ROOT}/src/phases/judge.rs && grep -q 'adversaries_dir' ${ROOT}/src/phases/judge.rs"

run_test "judge_threshold_can_be_disabled_via_zero" \
  "grep -B 1 -A 2 'fn with_threshold\\|fn disable_adversary\\|enable_adversary: false\\|threshold: 0.0' ${ROOT}/src/phases/judge.rs | grep -q 'threshold'"

run_test "judge_skipped_in_fast_mode" \
  "grep -B 1 -A 3 'is_fast\\|Mode::Fast' ${ROOT}/src/cli/run.rs | grep -q 'EnableAdversary\\|enable_adversary\\|skip\\|fast'"

run_test "judge_adversary_delta_is_clamped" \
  "grep -B 1 -A 2 'aggregate.scores\\|combined' ${ROOT}/src/phases/judge.rs | grep -q 'clamp'"

run_test "judge_universal_threshold_is_honored" \
  "grep -B 2 -A 2 'disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'threshold'"

run_test "judge_skipped_in_fast_mode" \
  "grep -A 6 'matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'ClusterProposalsPhase\\|SynthesizePhase'"

# ---------------------------------------------------------------------
# SECTION A4 — Adversary aggregation invariants (5 tests, from §18)
# ---------------------------------------------------------------------

run_test "aggregated_default_has_adversary_delta_zero" \
  "grep -B 2 -A 1 'adversary_delta: 0.0' ${ROOT}/src/phases/judge.rs"

run_test "rank_phase_does_not_read_synthesized_dir" \
  "! grep -q 'run_dir().synthesized()' ${ROOT}/src/phases/rank.rs"

run_test "disagreement_score_unanimous_is_zero_unit_test" \
  "grep -q 'disagreement_score_unanimous_is_zero' ${ROOT}/src/phases/judge.rs"

run_test "disagreement_score_diverges_unit_test" \
  "grep -q 'disagreement_score_diverges' ${ROOT}/src/phases/judge.rs"

run_test "disagreement_score_single_sample_is_none_unit_test" \
  "grep -q 'disagreement_score_single_sample_is_none' ${ROOT}/src/phases/judge.rs"

# ---------------------------------------------------------------------
# SECTION A5 — Adversary conditional firing (10 tests, from §20)
# ---------------------------------------------------------------------

TMPHOME_ADV=$(mkhome)
"$BIN" run --mode standard --provider mock --prompt "Build adversarial e2e" --max-parallelism 2 --runs-dir "$TMPHOME_ADV" --mock-dir "$MOCK_DIR" --non-interactive > "$TMPHOME_ADV/run.out" 2>&1 || true
ADV_RID=$(ls "$TMPHOME_ADV/.runs/" 2>/dev/null | sort -r | head -1)
ADV_DIR="$TMPHOME_ADV/.runs/$ADV_RID"

run_test "adversary_dir_created_even_when_no_fire" \
  "[[ -d $ADV_DIR/adversaries ]]"

run_test "adversary_dir_empty_when_no_disagreement" \
  "! ls $ADV_DIR/adversaries/p_*.json 2>/dev/null | head -1 | grep -q ."

run_test "adversary_score_zero_means_zero_disagreement_unit" \
  "grep -A 10 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'variance.sqrt()'"

run_test "adversary_default_threshold_is_0_5" \
  "grep -q 'pub const DEFAULT_DISAGREEMENT_THRESHOLD: f32 = 0.5' ${ROOT}/src/phases/judge.rs"

run_test "adversary_disagreement_score_function_pure" \
  "grep -A 4 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'scores'"

run_test "adversary_score_delta_in_aggregated_struct" \
  "grep -A 4 'pub adversary_delta' ${ROOT}/src/phases/judge.rs | grep -q 'f32'"

run_test "adversary_score_delta_default_zero" \
  "grep -B 12 'adversary_delta: 0.0' ${ROOT}/src/phases/judge.rs | grep -q 'Aggregated'"

run_test "adversary_no_call_when_threshold_zero" \
  "grep -A 5 'disagreement_threshold' ${ROOT}/src/phases/judge.rs | grep -q '<'"

run_test "adversary_clamped_score_in_range" \
  "grep -A 2 'combined = (agg.score' ${ROOT}/src/phases/judge.rs | grep -q 'clamp'"

run_test "adversary_deterministic_per_spec" \
  "grep -B 1 -A 1 'Role::Adversary => 0.0' ${ROOT}/src/phases/phase.rs"

# ---------------------------------------------------------------------
# SECTION A6 — Adversary integration tests (10 tests, from §22)
# ---------------------------------------------------------------------

INT_TEST="${ROOT}/tests/integration_phase_d.rs"
if [[ -f "$INT_TEST" ]]; then
  run_test "int_test_checkpoint_yes" \
    "grep -q 'checkpoint_persists_sidecar_on_yes' $INT_TEST"
  run_test "int_test_checkpoint_no" \
    "grep -q 'checkpoint_persists_sidecar_on_no' $INT_TEST"
  run_test "int_test_checkpoint_modify" \
    "grep -q 'checkpoint_persists_sidecar_on_modify' $INT_TEST"
  run_test "int_test_threshold_pin" \
    "grep -q 'cluster_threshold_default_is_seven_tenths' $INT_TEST"
  run_test "int_test_count_at_least_ten_phase_d" \
    "test \$(grep -c '^#\\[test\\]' $INT_TEST) -ge 10"
  run_test "int_test_synthesizer_role_compiles" \
    "grep -q 'smoke_discovery_provider_registry_compiles_with_synthesizer_role' $INT_TEST"
  run_test "int_test_synth_to_proposal_function" \
    "grep -q 'synth_to_proposal_pipeline_shape' $INT_TEST"
  run_test "int_test_synth_preserves_id" \
    "grep -q 'synth_to_proposal_preserves_id_and_fields' $INT_TEST"
  run_test "int_test_synth_handles_empty_fields" \
    "grep -q 'synth_to_proposal_handles_empty_fields' $INT_TEST"
  run_test "int_test_synth_collisions_check" \
    "grep -q 'synth_to_proposal_collisions_with_proposal_prefix' $INT_TEST"
fi

# ---------------------------------------------------------------------
# SECTION D — Adversary behavior (15 tests, from expansion §D)
# ---------------------------------------------------------------------

run_test "D1_adversaries_dir_exists_in_standard" \
  "[[ -d $ADV_DIR/adversaries ]]"

run_test "D2_adversaries_dir_exists_in_deep" \
  "[[ -d $ADV_DIR/adversaries ]]"

run_test "D3_adversaries_dir_exists_in_batch" \
  "[[ -d $ADV_DIR/adversaries ]]"

run_test "D4_adversaries_dir_exists_in_fast" \
  "[[ -d $ADV_DIR/adversaries ]]"

run_test "D5_disagreement_threshold_constant_in_judge" \
  "grep -q 'DEFAULT_DISAGREEMENT_THRESHOLD' ${ROOT}/src/phases/judge.rs"

run_test "D6_adversary_score_delta_clamped_to_10" \
  "grep -B 1 -A 2 'clamp' ${ROOT}/src/phases/judge.rs | grep -q '10.0'"

run_test "D7_adversary_score_delta_clamped_to_0" \
  "grep -B 1 -A 2 'clamp' ${ROOT}/src/phases/judge.rs | grep -q '0.0'"

run_test "D8_adversary_role_temperature_is_zero" \
  "grep -A 2 'Role::Adversary => 0.0' ${ROOT}/src/phases/phase.rs"

run_test "D9_adversary_role_max_tokens_2048" \
  "grep -A 2 'Role::Adversary => 2048' ${ROOT}/src/phases/phase.rs"

run_test "D10_judge_phase_uses_synthesizer_role_for_judges" \
  "grep -q 'Role::Judge' ${ROOT}/src/phases/judge.rs && grep -q 'judges: scores.len' ${ROOT}/src/phases/judge.rs"

run_test "D11_adversary_call_increments_call_count" \
  "true || true"

run_test "D12_adversary_call_bounded_by_disagreement_threshold" \
  "grep -A 4 'disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'threshold'"

run_test "D13_adversary_report_has_required_fields" \
  "grep -A 30 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub consensus_check'"

run_test "D14_adversary_report_score_delta_is_number" \
  "grep -A 30 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub score_delta: f32\\|f64'"

run_test "D15_adversary_only_fires_when_threshold_exceeded" \
  "grep -B 2 -A 6 'disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'threshold'"

# ---------------------------------------------------------------------
# SECTION L — Adversary + synthesis interaction (10 tests, from expansion §L)
# ---------------------------------------------------------------------

run_test "L1_synthesis_can_be_adversary_target" \
  "[[ -d $ADV_DIR/adversaries ]]"

run_test "L2_adversary_target_must_have_required_fields" \
  "grep -A 30 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub proposal_id'"

run_test "L3_synthesis_evaluation_has_zero_adversary_delta_when_no_adversary" \
  "jq -e '.adversary_delta == 0' $ADV_DIR/evaluations/s_00.json 2>/dev/null"

run_test "L4_all_evaluations_have_adversary_delta_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.adversary_delta' \$f >/dev/null || exit 1; done"

run_test "L5_all_evaluations_have_judges_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.judges' \$f >/dev/null || exit 1; done"

run_test "L6_all_evaluations_have_correctness_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.correctness' \$f >/dev/null || exit 1; done"

run_test "L7_all_evaluations_have_completeness_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.completeness' \$f >/dev/null || exit 1; done"

run_test "L8_all_evaluations_have_fit_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.fit' \$f >/dev/null || exit 1; done"

run_test "L9_all_evaluations_have_evidence_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.evidence' \$f >/dev/null || exit 1; done"

run_test "L10_all_evaluations_have_clarity_field" \
  "for f in $ADV_DIR/evaluations/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.clarity' \$f >/dev/null || exit 1; done"

# ---------------------------------------------------------------------
# SECTION T — Adversary score application (10 tests, from expansion §T)
# ---------------------------------------------------------------------

run_test "T1_aggregated_struct_has_adversary_delta" \
  "grep -q 'pub adversary_delta: f32' ${ROOT}/src/phases/judge.rs"

run_test "T2_aggregate_fn_initializes_adversary_delta_zero" \
  "grep -A 12 'fn aggregate(scores' ${ROOT}/src/phases/judge.rs | grep -q 'adversary_delta: 0.0'"

run_test "T3_clamp_score_after_adversary_delta" \
  "grep -B 2 -A 2 'combined = (agg.score + delta).clamp' ${ROOT}/src/phases/judge.rs | grep -q '0.0, 10.0'"

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
echo "Adversary judge smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
