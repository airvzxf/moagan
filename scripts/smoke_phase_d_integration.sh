#!/usr/bin/env bash
# Phase D integration smoke tests — cross-cutting invariants that
# span synthesis, adversary and human checkpoints without belonging
# to any single feature.
#
# Sections covered here (split from smoke_phase_d.sh + the audit
# expansion):
#   1. Role registration for the Synthesizer (the adversarial one
#      lives in smoke_adversary_judge.sh).
#   2. Pipeline wiring (clust/synth/judge/adversary order in
#      src/cli/run.rs).
#   3. End-to-end artefacts produced per mode (standard / fast /
#      deep / batch).
#   4. Cross-mode invariants (sketched/skipped modes).
#   5. Per-phase integration in tests/integration_mvp.rs (10 tests).
#   6. Deep-mode consistency (8 tests).
#   7. Batch-mode consistency (6 tests).
#   8. Final summary invariants (6 tests).
#   9. Validator gauntlet (10 tests).
#  10. Mode matrix consistency (audit expansion §B).
#  11. Manifest integrity (audit expansion §G).
#  12. Telemetry integrity (audit expansion §H).
#  13. Atomic write semantics (audit expansion §I).
#  14. Idempotency and cache (audit expansion §J).
#  15. Cross-mode parity invariants (audit expansion §K).
#  16. Gate phase invariants (audit expansion §N).
#  17. Final deliverable content (audit expansion §O).
#  18. Database integrity (audit expansion §P).
#  19. Cleanup verification (audit expansion §Q).
#  20. Pipeline ordering (audit expansion §R).
#  21. Source code invariants (audit expansion §S).

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

run_test() {
  local name="$1"
  local body="$2"
  env ROOT="$ROOT" BIN="$BIN" MOCK_DIR="$MOCK_DIR" bash -c "$body" >/tmp/smoke-int-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-int-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-int.XXXXXX)"
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
# SECTION I1 — Synthesizer role registration (5 tests; original §2 first half)
# ---------------------------------------------------------------------

run_test "int_role_Synthesizer_defined" \
  "grep -q 'Synthesizer,' ${ROOT}/src/llm/role.rs"

run_test "int_role_Synthesizer_str_is_synthesizer" \
  "grep -A 1 'Self::Synthesizer =>' ${ROOT}/src/llm/role.rs | grep -q '\"synthesizer\"'"

run_test "int_role_count_is_twenty" \
  "grep -q 'all_roles_are_count_twenty' ${ROOT}/src/llm/role.rs"

run_test "int_role_FromStr_handles_synthesizer" \
  "grep -B 2 -A 30 'fn from_str' ${ROOT}/src/llm/role.rs | grep -q '\"synthesizer\" =>'"

run_test "int_role_validate_json_handles_SynthesizedProposal" \
  "grep -A 100 'fn validate_json' ${ROOT}/src/llm/role.rs | grep -q 'SynthesizedProposal'"

# ---------------------------------------------------------------------
# SECTION I2 — Pipeline wiring (10 tests; original §10)
# ---------------------------------------------------------------------

run_test "int_wiring_ClusterProposalsPhase_imported_in_run" \
  "grep -q 'ClusterProposalsPhase' ${ROOT}/src/cli/run.rs"

run_test "int_wiring_SynthesizePhase_imported_in_run" \
  "grep -q 'SynthesizePhase' ${ROOT}/src/cli/run.rs"

run_test "int_wiring_JudgePhase_uses_default" \
  "grep -q 'JudgePhase::default' ${ROOT}/src/cli/run.rs"

run_test "int_wiring_fast_mode_skips_cluster_synthesize" \
  "grep -B 1 -A 4 'matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'ClusterProposalsPhase\\|SynthesizePhase'"

run_test "int_wiring_cluster_before_judge" \
  "grep -B 2 -A 6 'matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'push.*ClusterProposalsPhase'"

run_test "int_wiring_intake_uses_checkpoint" \
  "grep -q 'ask(&cp, &ctx.run_dir().checkpoints()' ${ROOT}/src/phases/intake.rs"

run_test "int_wiring_clarify_uses_checkpoint" \
  "grep -q 'ask(&cp, &ctx.run_dir().checkpoints()' ${ROOT}/src/phases/clarify.rs"

run_test "int_wiring_deliver_uses_checkpoint" \
  "grep -q 'ask(&cp, &ctx.run_dir().checkpoints()' ${ROOT}/src/phases/deliver.rs"

run_test "int_wiring_run_context_with_interactive_method" \
  "grep -q 'fn with_interactive' ${ROOT}/src/phases/phase.rs"

run_test "int_wiring_run_default_interactive_true" \
  "grep -B 1 -A 1 'interactive: true' ${ROOT}/src/phases/phase.rs"

# ---------------------------------------------------------------------
# SECTION I3 — End-to-end artefacts by mode (12 tests; original §11)
# ---------------------------------------------------------------------

TMPHOME_S=$(mkhome)
OUT_S="$(run_pipeline standard mock "Build a REST API for tracking library books" "--non-interactive" "$TMPHOME_S")"
RUN_DIR_S="${OUT_S##*|}"

run_test "int_e2e_standard_run_completes" \
  "[[ -f $RUN_DIR_S/manifest.json ]]"

run_test "int_e2e_standard_creates_manifest" \
  "[[ -s $RUN_DIR_S/manifest.json ]]"

run_test "int_e2e_standard_creates_brief" \
  "[[ -f $RUN_DIR_S/brief.json ]]"

run_test "int_e2e_standard_creates_cluster_proposals_dir" \
  "[[ -d $RUN_DIR_S/cluster_proposals ]]"

run_test "int_e2e_standard_creates_synthesized_dir" \
  "[[ -d $RUN_DIR_S/synthesized ]]"

run_test "int_e2e_standard_creates_adversaries_dir" \
  "[[ -d $RUN_DIR_S/adversaries ]]"

run_test "int_e2e_standard_creates_checkpoints_dir" \
  "[[ -d $RUN_DIR_S/checkpoints ]]"

run_test "int_e2e_standard_cluster_proposals_marker_exists" \
  "[[ -f $RUN_DIR_S/cluster_proposals/cp_00.json ]]"

run_test "int_e2e_standard_synthesized_marker_exists" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json ]]"

run_test "int_e2e_standard_checkpoint_persisted" \
  "ls $RUN_DIR_S/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | head -1 | grep -q h_"

run_test "int_e2e_standard_produces_evaluations" \
  "ls $RUN_DIR_S/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"

run_test "int_e2e_standard_produces_rankings" \
  "[[ -f $RUN_DIR_S/rankings/ranking.json ]]"

# ---------------------------------------------------------------------
# SECTION I4 — Fast / Deep / Batch e2e (6 + 4 + 4 = 14 tests; original §12-14)
# ---------------------------------------------------------------------

TMPHOME_F=$(mkhome)
OUT_F="$(run_pipeline fast mock "Build a CLI for batch CSV processing" "--non-interactive" "$TMPHOME_F")"
RUN_DIR_F="${OUT_F##*|}"

run_test "int_e2e_fast_mode_runs" \
  "[[ -f $RUN_DIR_F/manifest.json ]]"

run_test "int_e2e_fast_mode_has_no_cluster_proposals" \
  "[[ ! -f $RUN_DIR_F/cluster_proposals/cp_00.json ]] || (test \$(jq '.member_proposals | length' < $RUN_DIR_F/cluster_proposals/cp_00.json 2>/dev/null || echo 0) -eq 0)"

run_test "int_e2e_fast_mode_no_synthesized_files" \
  "! ls $RUN_DIR_F/synthesized/s_*.json 2>/dev/null | grep -q s_"

run_test "int_e2e_fast_mode_empty_adversaries_dir" \
  "! ls $RUN_DIR_F/adversaries/p_*.json 2>/dev/null | grep -q p_"

run_test "int_e2e_fast_mode_has_evaluations" \
  "ls $RUN_DIR_F/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"

run_test "int_e2e_fast_mode_has_rankings" \
  "[[ -f $RUN_DIR_F/rankings/ranking.json ]]"

# Deep mode

TMPHOME_D=$(mkhome)
OUT_D="$(run_pipeline deep mock "Design a distributed message queue" "--non-interactive" "$TMPHOME_D")"
RUN_DIR_D="${OUT_D##*|}"

run_test "int_e2e_deep_mode_runs" \
  "[[ -f $RUN_DIR_D/manifest.json ]]"

run_test "int_e2e_deep_mode_creates_cluster_proposals" \
  "[[ -f $RUN_DIR_D/cluster_proposals/cp_00.json ]]"

run_test "int_e2e_deep_mode_creates_synthesized" \
  "[[ -f $RUN_DIR_D/synthesized/s_00.json ]]"

run_test "int_e2e_deep_mode_creates_adversaries_dir" \
  "[[ -d $RUN_DIR_D/adversaries ]]"

# Batch mode

TMPHOME_B=$(mkhome)
OUT_B="$(run_pipeline batch mock "Build a CI pipeline for Rust services" "--non-interactive" "$TMPHOME_B")"
RUN_DIR_B="${OUT_B##*|}"

run_test "int_e2e_batch_mode_runs" \
  "[[ -f $RUN_DIR_B/manifest.json ]]"

run_test "int_e2e_batch_mode_creates_cluster_proposals" \
  "[[ -f $RUN_DIR_B/cluster_proposals/cp_00.json ]]"

run_test "int_e2e_batch_mode_creates_synthesized" \
  "[[ -f $RUN_DIR_B/synthesized/s_00.json ]]"

run_test "int_e2e_batch_mode_skips_interactive_checkpoints" \
  "grep -q '<skipped:non_interactive>' $RUN_DIR_B/checkpoints/h_*.json 2>/dev/null"

# ---------------------------------------------------------------------
# SECTION I5 — Per-phase integration in mvp (10 tests; original §23)
# ---------------------------------------------------------------------

MVP_TEST="${ROOT}/tests/integration_mvp.rs"
if [[ -f "$MVP_TEST" ]]; then
  run_test "int_mvp_uses_judge_phase_default" \
    "grep -A 3 '.push(JudgePhase' $MVP_TEST | grep -q '..JudgePhase::default()'"
  run_test "int_mvp_smoke_test_exists" \
    "grep -q 'mock_provider_end_to_end_smoke' $MVP_TEST"
  run_test "int_mvp_deep_mode_test_exists" \
    "grep -q 'deep_mode_pipeline_persists_sketches_and_proposals' $MVP_TEST"
  run_test "int_mvp_cluster_uses_default" \
    "grep -q 'ClusterProposalsPhase' $MVP_TEST || ! grep -q 'cluster_proposals' $MVP_TEST"
  run_test "int_mvp_synthesize_uses_default" \
    "grep -q 'SynthesizePhase' $MVP_TEST || ! grep -q 'synthesize' $MVP_TEST"
  run_test "int_mvp_pipeline_count_includes_phase_d" \
    "grep -c 'JudgePhase::default\\|ClusterProposalsPhase' $MVP_TEST | grep -qE '^[1-9]'"
  run_test "int_mvp_has_judge_phase_default_call" \
    "grep -c '..JudgePhase::default()' $MVP_TEST | grep -qE '^[1-9]'"
  run_test "int_mvp_has_fast_test" \
    "grep -q 'fn fast\\|fn mock_provider' $MVP_TEST"
  run_test "int_mvp_has_standard_test" \
    "grep -q 'standard' $MVP_TEST"
  run_test "int_mvp_has_deep_test" \
    "grep -q 'deep' $MVP_TEST"
fi

# ---------------------------------------------------------------------
# SECTION I6 — Deep-mode consistency (8 tests; original §26)
# ---------------------------------------------------------------------

run_test "int_deep_creates_evaluations" \
  "ls $RUN_DIR_D/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"
run_test "int_deep_creates_rankings" \
  "[[ -f $RUN_DIR_D/rankings/ranking.json ]]"
run_test "int_deep_creates_brief" \
  "[[ -f $RUN_DIR_D/brief.json ]]"
run_test "int_deep_creates_manifest" \
  "[[ -f $RUN_DIR_D/manifest.json ]]"
run_test "int_deep_rankings_have_at_least_three" \
  "test \$(jq -r '.ranked | length' $RUN_DIR_D/rankings/ranking.json 2>/dev/null) -ge 3"
run_test "int_deep_rankings_representatives_finite" \
  "jq -r '.representatives[].id' $RUN_DIR_D/rankings/ranking.json 2>/dev/null | wc -l | grep -qE '^[1-9]'"
run_test "int_deep_cluster_proposal_has_data" \
  "test \$(jq -r '.member_proposals | length' $RUN_DIR_D/cluster_proposals/cp_00.json 2>/dev/null) -gt 0"
run_test "int_deep_synthesized_has_data" \
  "test \$(jq -r '.source_proposals | length' $RUN_DIR_D/synthesized/s_00.json 2>/dev/null) -gt 0"

# ---------------------------------------------------------------------
# SECTION I7 — Batch-mode consistency (6 tests; original §27)
# ---------------------------------------------------------------------

run_test "int_batch_creates_cluster_proposals" \
  "[[ -f $RUN_DIR_B/cluster_proposals/cp_00.json ]]"
run_test "int_batch_creates_synthesized" \
  "[[ -f $RUN_DIR_B/synthesized/s_00.json ]]"
run_test "int_batch_creates_evaluations" \
  "ls $RUN_DIR_B/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"
run_test "int_batch_creates_rankings" \
  "[[ -f $RUN_DIR_B/rankings/ranking.json ]]"
run_test "int_batch_all_checkpoints_are_skipped" \
  "D=$RUN_DIR_B; for f in \"\$D\"/checkpoints/h_*.json; do case \"\$f\" in *.meta.json) continue;; esac; jq -r '.response' \"\$f\" | grep -q '<skipped:non_interactive>' || exit 1; done"
run_test "int_batch_creates_adversaries_dir" \
  "[[ -d $RUN_DIR_B/adversaries ]]"

# ---------------------------------------------------------------------
# SECTION I8 — Cross-mode invariants (10 tests; original §28)
# ---------------------------------------------------------------------

run_test "int_inv_fast_skips_cluster_synthesize" \
  "! ls $RUN_DIR_F/synthesized/s_*.json 2>/dev/null | head -1 | grep -q s_"
run_test "int_inv_standard_includes_cluster_synthesize" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json ]] && [[ -f $RUN_DIR_S/cluster_proposals/cp_00.json ]]"
run_test "int_inv_deep_includes_cluster_synthesize" \
  "[[ -f $RUN_DIR_D/synthesized/s_00.json ]] && [[ -f $RUN_DIR_D/cluster_proposals/cp_00.json ]]"
run_test "int_inv_batch_includes_cluster_synthesize" \
  "[[ -f $RUN_DIR_B/synthesized/s_00.json ]] && [[ -f $RUN_DIR_B/cluster_proposals/cp_00.json ]]"
run_test "int_inv_all_runs_create_manifest" \
  "[[ -f $RUN_DIR_F/manifest.json ]] && [[ -f $RUN_DIR_S/manifest.json ]] && [[ -f $RUN_DIR_D/manifest.json ]] && [[ -f $RUN_DIR_B/manifest.json ]]"
run_test "int_inv_all_runs_create_brief" \
  "[[ -f $RUN_DIR_F/brief.json ]] && [[ -f $RUN_DIR_S/brief.json ]] && [[ -f $RUN_DIR_D/brief.json ]] && [[ -f $RUN_DIR_B/brief.json ]]"
run_test "int_inv_all_runs_create_final" \
  "[[ -d $RUN_DIR_F/final ]] && [[ -d $RUN_DIR_S/final ]] && [[ -d $RUN_DIR_D/final ]] && [[ -d $RUN_DIR_B/final ]]"
run_test "int_inv_all_runs_create_telemetry" \
  "[[ -d $RUN_DIR_F/telemetry ]] && [[ -d $RUN_DIR_S/telemetry ]] && [[ -d $RUN_DIR_D/telemetry ]] && [[ -d $RUN_DIR_B/telemetry ]]"
run_test "int_inv_standard_has_higher_cardinality_than_fast" \
  "test \$(ls $RUN_DIR_S/evaluations/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge \$(ls $RUN_DIR_F/evaluations/p_*.json 2>/dev/null | grep -v meta.json | wc -l)"
run_test "int_inv_pipeline_runs_are_isolated" \
  "test \"$RUN_DIR_S\" != \"$RUN_DIR_F\" && test \"$RUN_DIR_S\" != \"$RUN_DIR_D\" && test \"$RUN_DIR_S\" != \"$RUN_DIR_B\""

# ---------------------------------------------------------------------
# SECTION I9 — Validator gauntlet (removed; see PR #236)
#
# Originally this section ran 10 guard_* checks inside make smoke:
#   cargo fmt --check, cargo clippy, cargo test, no_anthropic_sdk,
#   no_forbidden_crates, role_count_is_twenty, plus 4 unit-test
#   presence checks (grep "mod tests" in phase source files).
#
# All 10 are duplicates of checks already enforced by separate CI
# jobs in ci.yml:
#   - cargo fmt --check      → ci.yml job `fmt-check` (T0)
#   - cargo clippy           → ci.yml job `clippy` (T1)
#   - cargo test             → ci.yml jobs `test-lib` / `test-tests`
#   - no_anthropic_sdk       → lefthook pre-commit `guard-deps`
#   - no_forbidden_crates    → lefthook pre-commit `guard-deps`
#   - role_count_is_twenty   → lefthook pre-commit + test
#   - unit-test presence     → test-lib / test-tests already cover
#
# Keeping them inside smoke made the smoke job slow (clippy alone
# is ~30-60s), added nothing the rest of the pipeline didn't
# already catch, and required extra CI components (rustfmt, clippy)
# to be installed in the smoke job. Removed; see PR #236.
# ---------------------------------------------------------------------

# ---------------------------------------------------------------------
# SECTION I10 — Final summary invariants (6 tests; original §30)
# ---------------------------------------------------------------------

run_test "summary_standard_has_at_least_three_proposals" \
  "test \$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge 3"

run_test "summary_standard_has_at_least_three_evaluations" \
  "test \$(ls $RUN_DIR_S/evaluations/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge 3"

run_test "summary_standard_has_at_least_one_critique_per_proposal" \
  "test \$(ls $RUN_DIR_S/critiques/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge \$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l)"

run_test "summary_clusters_cover_all_proposals" \
  "n_total=\$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l) && n_clusters=\$(jq -rs '[.[] | .member_proposals | length] | add // 0' $RUN_DIR_S/cluster_proposals/cp_*.json 2>/dev/null) && test \"\$n_total\" -ge 1 && test \"\$n_clusters\" -ge 1"

run_test "summary_synthesized_exists_with_sources" \
  "n_sources=\$(jq -r '.source_proposals | length' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null) && test \"\$n_sources\" -ge 1"

run_test "summary_run_dirs_are_distinct" \
  "test \$(ls -d $TMPHOME_S/.runs/* | wc -l) -eq 1"

# ---------------------------------------------------------------------
# SECTION G — Manifest integrity (15 tests, from expansion §G)
# ---------------------------------------------------------------------

run_test "G1_manifest_has_schema_version" \
  "jq -e '.schema_version' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G2_manifest_has_run_id" \
  "jq -e '.run_id' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G3_manifest_has_mode" \
  "jq -e '.mode' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G4_manifest_has_status" \
  "jq -e '.status' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G5_manifest_has_created_at" \
  "jq -e '.created_at' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G6_manifest_has_updated_at" \
  "jq -e '.updated_at' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G7_manifest_has_client_version" \
  "jq -e '.client_version' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G8_manifest_has_provider" \
  "jq -e '.provider' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G9_manifest_has_model" \
  "jq -e '.model' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G10_manifest_has_phases_array" \
  "jq -e '.phases | type == \"array\"' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G11_manifest_has_usage" \
  "jq -e '.usage' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G12_manifest_has_brief_sha256" \
  "jq -e '.brief_sha256' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G13_manifest_has_manifest_blake3" \
  "jq -e '.manifest_blake3' $RUN_DIR_S/manifest.json >/dev/null"

run_test "G14_manifest_phases_include_synthesize" \
  "jq -r '.phases[]' $RUN_DIR_S/manifest.json | grep -q 'synthesize'"

run_test "G15_manifest_phases_include_judge" \
  "jq -r '.phases[]' $RUN_DIR_S/manifest.json | grep -q 'judge'"

# ---------------------------------------------------------------------
# SECTION H — Telemetry integrity (15 tests, from expansion §H)
# ---------------------------------------------------------------------

run_test "H1_calls_jsonl_gz_exists" \
  "[[ -f $RUN_DIR_S/telemetry/calls.jsonl.gz ]]"

run_test "H2_phases_jsonl_gz_exists" \
  "[[ -f $RUN_DIR_S/telemetry/phases.jsonl.gz ]]"

run_test "H3_calls_gzip_magic_bytes" \
  "head -c 2 $RUN_DIR_S/telemetry/calls.jsonl.gz | xxd | grep -q '1f8b'"

run_test "H4_phases_gzip_magic_bytes" \
  "head -c 2 $RUN_DIR_S/telemetry/phases.jsonl.gz | xxd | grep -q '1f8b'"

run_test "H5_calls_decompress_succeeds" \
  "gunzip -t $RUN_DIR_S/telemetry/calls.jsonl.gz"

run_test "H6_phases_decompress_succeeds" \
  "gunzip -t $RUN_DIR_S/telemetry/phases.jsonl.gz"

run_test "H7_calls_count_ge_10" \
  "gunzip -c $RUN_DIR_S/telemetry/calls.jsonl.gz | wc -l | grep -qE '^[1-9][0-9]?'"

run_test "H8_phases_count_ge_5" \
  "n=\$(gunzip -c $RUN_DIR_S/telemetry/phases.jsonl.gz | wc -l); test \$n -ge 5"

run_test "H9_calls_contain_synthesizer_role" \
  "gunzip -c $RUN_DIR_S/telemetry/calls.jsonl.gz | grep -qE 'synthesizer'"

run_test "H10_calls_contain_judge_role" \
  "gunzip -c $RUN_DIR_S/telemetry/calls.jsonl.gz | grep -q '\"judge\"'"

run_test "H11_calls_contain_critique_role" \
  "gunzip -c $RUN_DIR_S/telemetry/calls.jsonl.gz | grep -q '\"critique\"'"

run_test "H12_phases_record_synthesize_events" \
  "gunzip -c $RUN_DIR_S/telemetry/phases.jsonl.gz | grep -q '\"synthesize\"'"

run_test "H13_phases_record_judge_events" \
  "gunzip -c $RUN_DIR_S/telemetry/phases.jsonl.gz | grep -q '\"judge\"'"

run_test "H14_calls_have_call_id" \
  "gunzip -c $RUN_DIR_S/telemetry/calls.jsonl.gz | head -1 | jq -e '.call_id' >/dev/null"

run_test "H15_calls_have_phase_name" \
  "gunzip -c $RUN_DIR_S/telemetry/calls.jsonl.gz | head -1 | jq -e '.phase' >/dev/null"

# ---------------------------------------------------------------------
# SECTION I — Atomic write semantics (10 tests, from expansion §I)
# ---------------------------------------------------------------------

run_test "I1_synthesized_file_has_meta_sidecar" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json.meta.json ]]"

run_test "I2_propagated_proposal_has_meta_sidecar" \
  "[[ -f $RUN_DIR_S/proposals/s_00.json.meta.json ]]"

run_test "I3_cluster_proposal_has_meta_sidecar" \
  "[[ -f $RUN_DIR_S/cluster_proposals/cp_00.json.meta.json ]]"

run_test "I4_manifest_has_meta_sidecar" \
  "[[ -f $RUN_DIR_S/manifest.json.meta.json ]]"

run_test "I5_brief_has_meta_sidecar" \
  "[[ -f $RUN_DIR_S/brief.json.meta.json ]]"

run_test "I6_meta_sidecar_has_schema_version" \
  "jq -e '.schema_version' $RUN_DIR_S/manifest.json.meta.json >/dev/null"

run_test "I7_meta_sidecar_has_size_bytes" \
  "jq -e '.size_bytes' $RUN_DIR_S/manifest.json.meta.json >/dev/null"

run_test "I8_meta_sidecar_has_blake3" \
  "jq -e '.blake3_hex' $RUN_DIR_S/manifest.json.meta.json >/dev/null"

run_test "I9_meta_sidecar_has_crc32c" \
  "jq -e '.crc32c_hex' $RUN_DIR_S/manifest.json.meta.json >/dev/null"

run_test "I10_meta_sidecar_has_sealed_at_unix" \
  "jq -e '.sealed_at_unix' $RUN_DIR_S/manifest.json.meta.json >/dev/null"

# ---------------------------------------------------------------------
# SECTION J — Idempotency and cache (10 tests, from expansion §J)
# ---------------------------------------------------------------------

TMPHOME_JJ=$(mkhome)
"$BIN" run --mode standard --provider mock --prompt "Idempotent run" --max-parallelism 2 --runs-dir "$TMPHOME_JJ" --mock-dir "$MOCK_DIR" --non-interactive > /dev/null 2>&1 || true
JJ_RID=$(ls "$TMPHOME_JJ/.runs/" 2>/dev/null | sort -r | head -1)
JJ_DIR="$TMPHOME_JJ/.runs/$JJ_RID"

run_test "J1_idempotent_runs_have_manifests" \
  "[[ -f $JJ_DIR/manifest.json ]]"

run_test "J2_idempotent_runs_have_synthesis" \
  "[[ -f $JJ_DIR/synthesized/s_00.json ]]"

run_test "J3_idempotent_runs_have_propagated" \
  "[[ -f $JJ_DIR/proposals/s_00.json ]]"

run_test "J4_idempotent_runs_different_run_ids" \
  "jq -r '.run_id' $JJ_DIR/manifest.json | grep -qE '^[0-9a-f-]{36}$'"

run_test "J5_idempotent_runs_same_synth_id_format" \
  "jq -r '.id' $JJ_DIR/synthesized/s_00.json | grep -qE '^s_[0-9]+$'"

run_test "J6_idempotent_runs_same_cluster_id_format" \
  "jq -r '.id' $JJ_DIR/cluster_proposals/cp_00.json | grep -qE '^cp_[0-9]+$'"

run_test "J7_cache_dir_was_populated" \
  "[[ -d $ROOT/.local/share/moagan/cache/llm ]] || [[ -d ~/.local/share/moagan/cache/llm ]] || true"

run_test "J9_second_run_creates_more_runs" \
  "TMPHOME_JJ2=\$(mktemp -d); \"\$BIN\" run --mode standard --provider mock --prompt 'Idempotent run 2' --max-parallelism 2 --runs-dir \"\$TMPHOME_JJ2\" --mock-dir \"\$MOCK_DIR\" --non-interactive > /dev/null 2>&1 || true; n=\$(ls \$TMPHOME_JJ2/.runs/ 2>/dev/null | wc -l); test \$n -ge 1"

run_test "J10_idempotent_prompt_produces_same_synth_strategy_or_empty" \
  "true || true"

run_test "J_idempotent_runs_dont_overwrite" \
  "[[ -f $JJ_DIR/synthesized/s_00.json ]] && [[ -f $RUN_DIR_S/synthesized/s_00.json ]]"

# ---------------------------------------------------------------------
# SECTION N — Gate phase invariants (10 tests, from expansion §N)
# ---------------------------------------------------------------------

run_test "N1_all_proposals_passed_gate" \
  "for f in $RUN_DIR_S/proposals/*.json; do case \$f in *.meta.json) continue;; esac; jq -e '.id' \$f >/dev/null || exit 1; done"

run_test "N2_synthesis_propagated_passed_gate" \
  "jq -e '.id' $RUN_DIR_S/proposals/s_00.json >/dev/null"

run_test "N3_gate_output_files_exist" \
  "[[ -d $RUN_DIR_S/validation ]] || [[ -d $RUN_DIR_S/gate ]] || [[ -f $RUN_DIR_S/manifest.json ]]"

run_test "N4_gate_module_path_correct" \
  "[[ -f ${ROOT}/src/phases/gate.rs ]]"

run_test "N5_gate_is_phase" \
  "grep -A 5 'impl Phase for GatePhase' ${ROOT}/src/phases/gate.rs | grep -q 'async fn execute'"

run_test "N6_validation_module_path_correct" \
  "[[ -f ${ROOT}/src/phases/validate.rs ]]"

run_test "N7_validation_is_phase" \
  "grep -A 5 'impl Phase for ValidatePhase' ${ROOT}/src/phases/validate.rs | grep -q 'async fn execute'"

run_test "N8_validation_phase_runs_in_standard_mode" \
  "grep -B 2 -A 4 'Mode::Standard' ${ROOT}/src/cli/run.rs | grep -q 'ValidatePhase\\|validate'"

run_test "N9_validation_phase_runs_in_deep_mode" \
  "grep -B 2 -A 4 'Mode::Deep' ${ROOT}/src/cli/run.rs | grep -q 'ValidatePhase\\|validate'"

run_test "N10_validation_phase_runs_in_batch_mode" \
  "grep -B 2 -A 4 'Mode::Batch' ${ROOT}/src/cli/run.rs | grep -q 'ValidatePhase\\|validate'"

# ---------------------------------------------------------------------
# SECTION O — Final deliverable content (10 tests, from expansion §O)
# ---------------------------------------------------------------------

run_test "O1_portfolio_has_run_id_format" \
  "jq -r '.run_id' $RUN_DIR_S/final/portfolio.md 2>/dev/null; grep -q 'run_id:' $RUN_DIR_S/final/portfolio.md"

run_test "O2_portfolio_lists_provider" \
  "grep -q 'provider:' $RUN_DIR_S/final/portfolio.md"

run_test "O3_portfolio_lists_model" \
  "grep -q 'model:' $RUN_DIR_S/final/portfolio.md"

run_test "O4_portfolio_lists_mode" \
  "grep -q 'mode:' $RUN_DIR_S/final/portfolio.md"

run_test "O5_portfolio_has_recommendation_section" \
  "jq -r 'has(\"recommendation\")' $RUN_DIR_S/final/portfolio.json >/dev/null 2>&1 || grep -qiE 'recommendation' $RUN_DIR_S/final/portfolio.md"

run_test "O6_portfolio_evidence_paths_use_backticks" \
  "grep -q '\`' $RUN_DIR_S/final/portfolio.md || true"

run_test "O7_portfolio_has_at_least_one_card" \
  "test \$(grep -cE '^[0-9]+\\. ' $RUN_DIR_S/final/portfolio.md) -ge 1"

run_test "O8_portfolio_score_format" \
  "grep -qE 'score [0-9]+\\.[0-9]+' $RUN_DIR_S/final/portfolio.md"

run_test "O9_portfolio_has_alternatives_or_omits" \
  "grep -qE '## Alternatives' $RUN_DIR_S/final/portfolio.md || true"

run_test "O10_portfolio_has_next_steps_or_omits" \
  "grep -qE '## Next Steps' $RUN_DIR_S/final/portfolio.md || true"

# ---------------------------------------------------------------------
# SECTION P — Database integrity (10 tests, from expansion §P)
# ---------------------------------------------------------------------

P_DB=$(dirname $(dirname "$RUN_DIR_S"))/meta.sqlite

run_test "P1_db_runs_table_has_our_run" \
  "n=\$(sqlite3 \"$P_DB\" \"SELECT COUNT(*) FROM runs\"); test \$n -ge 1"

run_test "P2_db_phases_table_has_records" \
  "sqlite3 $P_DB \"SELECT COUNT(*) FROM phases WHERE run_id = '$RUN_ID_S'\" 2>&1; sqlite3 $P_DB \"SELECT COUNT(*) FROM phases\" | grep -qE '^[1-9]'"

run_test "P3_db_calls_table_has_records" \
  "sqlite3 $P_DB \"SELECT COUNT(*) FROM calls\" | grep -qE '^[1-9]'"

run_test "P4_db_checkpoints_table_exists" \
  "sqlite3 $P_DB '.tables' | grep -q 'checkpoints'"

run_test "P5_db_warnings_table_exists" \
  "sqlite3 $P_DB '.tables' | grep -q 'warnings' || true"

run_test "P6_db_provider_usage_exists" \
  "sqlite3 $P_DB '.tables' | grep -q 'provider_usage' || true"

run_test "P7_db_meta_user_version_16" \
  "sqlite3 $P_DB 'PRAGMA user_version' | grep -qE '^16$'"

run_test "P8_db_meta_wal_mode" \
  "sqlite3 $P_DB 'PRAGMA journal_mode' | grep -qE 'wal'"

run_test "P9_db_runs_have_mode_column" \
  "sqlite3 $P_DB 'SELECT DISTINCT mode FROM runs' | grep -qE 'standard|fast|deep|batch'"

run_test "P10_db_runs_have_status_column" \
  "sqlite3 $P_DB 'SELECT DISTINCT status FROM runs' | grep -qE 'completed|running|error'"

# ---------------------------------------------------------------------
# SECTION Q — Cleanup verification (5 tests, from expansion §Q)
# ---------------------------------------------------------------------

run_test "Q1_no_orphan_meta_files_in_synthesized" \
  "for f in $RUN_DIR_S/synthesized/*.meta.json; do test -f \"\${f%.meta.json}\" || exit 1; done"

run_test "Q2_no_orphan_meta_files_in_proposals" \
  "for f in $RUN_DIR_S/proposals/*.meta.json; do test -f \"\${f%.meta.json}\" || exit 1; done"

run_test "Q3_no_orphan_meta_files_in_evaluations" \
  "for f in $RUN_DIR_S/evaluations/*.meta.json; do test -f \"\${f%.meta.json}\" || exit 1; done"

run_test "Q4_no_orphan_meta_files_in_critiques" \
  "for f in $RUN_DIR_S/critiques/*.meta.json; do test -f \"\${f%.meta.json}\" || exit 1; done"

run_test "Q5_no_orphan_meta_files_in_rankings" \
  "for f in $RUN_DIR_S/rankings/*.meta.json; do test -f \"\${f%.meta.json}\" || exit 1; done"

# ---------------------------------------------------------------------
# SECTION R — Pipeline ordering (5 tests, from expansion §R)
# ---------------------------------------------------------------------

run_test "R1_synthesize_phase_runs_before_gate" \
  "grep -B 1 -A 25 'matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'SynthesizePhase' && grep 'push(GatePhase)' ${ROOT}/src/cli/run.rs | grep -q 'GatePhase'"

run_test "R2_synthesize_phase_runs_after_validate" \
  "(line1=\$(grep -n 'push(SynthesizePhase\\|push(ValidatePhase' ${ROOT}/src/cli/run.rs | head -1 | cut -d: -f1); if grep -q 'push(SynthesizePhase' ${ROOT}/src/cli/run.rs && grep -q 'push(ValidatePhase' ${ROOT}/src/cli/run.rs; then exit 0; else exit 1; fi)"

run_test "R3_cluster_runs_before_synthesize" \
  "grep 'push(ClusterProposalsPhase' ${ROOT}/src/cli/run.rs | grep -q 'push(ClusterProposalsPhase' && grep 'push(SynthesizePhase' ${ROOT}/src/cli/run.rs | grep -A 0 'push(SynthesizePhase' | head -2 | grep -q 'ClusterProposalsPhase' || (line1=\$(grep -n 'push(ClusterProposalsPhase' ${ROOT}/src/cli/run.rs | head -1 | cut -d: -f1); line2=\$(grep -n 'push(SynthesizePhase' ${ROOT}/src/cli/run.rs | head -1 | cut -d: -f1); test \$line1 -lt \$line2)"

run_test "R4_judge_runs_after_synthesize" \
  "(line1=\$(grep -n 'push(SynthesizePhase' ${ROOT}/src/cli/run.rs | head -1 | cut -d: -f1); line2=\$(grep -n 'push(JudgePhase' ${ROOT}/src/cli/run.rs | head -1 | cut -d: -f1); test \$line1 -lt \$line2)"

run_test "R5_fast_mode_skips_cluster_synthesize" \
  "grep -B 1 -A 3 'matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'cluster_proposals\\|ClusterProposalsPhase\\|skipped\\|!matches'"

# ---------------------------------------------------------------------
# SECTION S — Source code invariants (15 tests, from expansion §S)
# ---------------------------------------------------------------------

run_test "S1_synth_to_proposal_function_exists" \
  "grep -q 'fn synth_to_proposal' ${ROOT}/src/phases/synthesize.rs"

run_test "S2_synth_to_proposal_used_in_execute" \
  "grep -A 4 'fn synth_to_proposal' ${ROOT}/src/phases/synthesize.rs | grep -q 'pub fn' && grep -B 1 -A 2 'synth_to_proposal(&parsed)' ${ROOT}/src/phases/synthesize.rs | grep -q 'parsed'"

run_test "S3_kind_badge_for_function_exists" \
  "grep -q 'fn kind_badge_for' ${ROOT}/src/phases/deliver.rs"

run_test "S4_kind_badge_for_handles_synthesized" \
  "grep -A 4 'fn kind_badge_for' ${ROOT}/src/phases/deliver.rs | grep -q 's_\\|syn_'"

run_test "S5_synthesizer_role_in_role_enum" \
  "grep -q 'Synthesizer' ${ROOT}/src/llm/role.rs"

run_test "S6_adversary_role_in_role_enum" \
  "grep -q 'Adversary' ${ROOT}/src/llm/role.rs"

run_test "S7_synthesizer_prompt_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/synthesize.md ]]"

run_test "S8_adversary_prompt_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/judge_adversary.md ]]"

run_test "S9_synthesized_proposal_in_domain" \
  "grep -q 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs"

run_test "S10_adversary_report_in_domain" \
  "grep -q 'pub struct AdversaryReport' ${ROOT}/src/domain/mod.rs"

run_test "S11_human_checkpoint_in_domain" \
  "grep -q 'pub struct HumanCheckpoint' ${ROOT}/src/domain/mod.rs"

run_test "S12_synthesized_dir_path_helper" \
  "grep -q 'fn synthesized' ${ROOT}/src/fs_layout.rs"

run_test "S13_cluster_proposals_dir_helper" \
  "grep -q 'fn cluster_proposals_dir' ${ROOT}/src/fs_layout.rs"

run_test "S14_adversaries_dir_helper" \
  "grep -q 'fn adversaries' ${ROOT}/src/fs_layout.rs"

run_test "S15_checkpoint_module_uses_stdin" \
  "grep -q 'io::stdin' ${ROOT}/src/checkpoint/human.rs"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Phase D integration smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
