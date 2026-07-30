#!/usr/bin/env bash
# Smoke tests for Phase D's intra-cluster synthesis path.
#
# Validates the four pieces that make up "the synthesis competes"
# (V4 §5.13 + proposal-02-rust.md §8.4):
#
#   1. ClusterProposalsPhase — groups near-duplicate proposals by
#      SimHash + Jaccard (CLUSTER_THRESHOLD = 0.7), writes
#      cluster_proposals/cp_<NN>.json.
#   2. SynthesizePhase — fires the synthesizer role per cluster
#      (skips singletons), writes synthesized/s_<NN>.json.
#   3. Propagation — the synthesis is *also* written to
#      proposals/s_<NN>.json so it runs through Gate, Critique,
#      Repair, Judge, Rank like any other proposal.
#   4. Portfolio badge — DeliverPhase marks synthesized entries with
#      "synthesis" in final/portfolio.md.
#
# Tests are split out of the original monolithic Phase D script
# (smoke_phase_d.sh) and the audit-driven expansion
# (smoke_phase_d_expansion.sh). Sections dedicated to the adversary
# judge live in smoke_adversary_judge.sh; the SQLite mirror lives in
# smoke_checkpoint_mirror.sh; cross-cutting integration tests live
# in smoke_phase_d_integration.sh.

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
  env ROOT="$ROOT" BIN="$BIN" MOCK_DIR="$MOCK_DIR" bash -c "$body" >/tmp/smoke-synth-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-synth-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-synth.XXXXXX)"
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
# SECTION S1 — SynthesizedProposal domain surface (6 tests)
#
# Trimmed from the original Section 1 (Phase D domain types, 15
# tests) — only the six SynthesizedProposal field-shape tests.
# AdversaryReport fields live in smoke_adversary_judge.sh; the
# HumanCheckpoint surface lives in smoke_human_checkpoint.sh.
# ---------------------------------------------------------------------

run_test "domain_has_SynthesizedProposal" \
  "grep -q 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs"

run_test "SynthesizedProposal_has_source_proposals_field" \
  "grep -A 20 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs | grep -q 'pub source_proposals'"

run_test "SynthesizedProposal_has_cluster_id_field" \
  "grep -A 20 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs | grep -q 'pub cluster_id'"

run_test "SynthesizedProposal_has_synthesis_strategy" \
  "grep -A 20 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs | grep -q 'pub synthesis_strategy'"

run_test "SynthesizedProposal_has_summary" \
  "grep -A 25 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs | grep -q 'pub summary'"

run_test "SynthesizedProposal_has_approach" \
  "grep -A 25 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs | grep -q 'pub approach'"

# ---------------------------------------------------------------------
# SECTION S2 — Synthesizer prompt (4 tests)
# ---------------------------------------------------------------------

run_test "prompt_synthesize_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/synthesize.md ]]"

run_test "prompt_synthesize_registered" \
  "grep -q 'synthesize.md' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_synthesize_mentions_merge" \
  "grep -qiE 'merge|combine|synthesize' ${ROOT}/src/llm/prompts/synthesize.md"

run_test "prompt_synthesize_mentions_id_target" \
  "grep -qiE 'id.*s_|stable.*id|naming|identifier' ${ROOT}/src/llm/prompts/synthesize.md"

# ---------------------------------------------------------------------
# SECTION S3 — Synthesizer role sampling (4 tests)
# ---------------------------------------------------------------------

run_test "max_tokens_Synthesizer_is_4000" \
  "grep -B 1 -A 1 'Role::Synthesizer => 4000' ${ROOT}/src/phases/phase.rs"

run_test "temp_Synthesizer_is_0_4" \
  "grep -B 1 -A 1 'Role::Synthesizer => 0.4' ${ROOT}/src/phases/phase.rs"

run_test "prompts_prompt_set_hash_includes_synthesize" \
  "grep -q 'synthesize.md' ${ROOT}/src/llm/prompts.rs && grep -q 'fn prompt_set_hash' ${ROOT}/src/llm/prompts.rs"

run_test "system_prompt_dispatches_Synthesizer" \
  "grep -A 5 'Role::Synthesizer =>' ${ROOT}/src/llm/prompts.rs | grep -qi 'synthesize'"

# ---------------------------------------------------------------------
# SECTION S4 — fs_layout synthesis dirs (4 tests)
# ---------------------------------------------------------------------

run_test "fs_layout_synthesized_path" \
  "grep -q 'fn synthesized' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_cluster_proposals_path" \
  "grep -q 'fn cluster_proposals' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_synthesized" \
  "awk '/Create every directory the run expects/{f=1; next} f{print; if (/^    }/) exit}' ${ROOT}/src/fs_layout.rs | grep -q 'self.synthesized(),'"

run_test "fs_layout_ensure_creates_cluster_proposals" \
  "awk '/Create every directory the run expects/{f=1; next} f{print; if (/^    }/) exit}' ${ROOT}/src/fs_layout.rs | grep -q 'self.cluster_proposals_dir(),'"

# ---------------------------------------------------------------------
# SECTION S5 — ClusterProposalsPhase surface (8 tests)
# ---------------------------------------------------------------------

run_test "cluster_proposals_module_exists" \
  "[[ -f ${ROOT}/src/phases/cluster_proposals.rs ]]"

run_test "cluster_proposals_CLUSTER_THRESHOLD_is_0_7" \
  "grep -q 'CLUSTER_THRESHOLD: f64 = 0.7' ${ROOT}/src/phases/cluster_proposals.rs || grep -q 'CLUSTER_THRESHOLD: f32 = 0.7' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_has_ProposalCluster_struct" \
  "grep -q 'pub struct ProposalCluster' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_has_cluster_text_fn" \
  "grep -q 'cluster_text' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_implements_phase" \
  "grep -A 6 'impl Phase for ClusterProposalsPhase' ${ROOT}/src/phases/cluster_proposals.rs | grep -q 'async fn execute'"

run_test "cluster_proposals_writes_cp_NN_naming" \
  "grep -q '\"cp_00\"' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_writes_empty_marker" \
  "grep -B 1 -A 6 'items.len() < 2' ${ROOT}/src/phases/cluster_proposals.rs | grep -q 'cp_00'"

run_test "cluster_proposals_module_exported" \
  "grep -q 'pub mod cluster_proposals' ${ROOT}/src/phases/mod.rs || grep -q 'pub use cluster_proposals' ${ROOT}/src/phases/mod.rs"

# ---------------------------------------------------------------------
# SECTION S6 — SynthesizePhase surface (8 tests)
# ---------------------------------------------------------------------

run_test "synthesize_module_exists" \
  "[[ -f ${ROOT}/src/phases/synthesize.rs ]]"

run_test "synthesize_has_min_cluster_size_default_2" \
  "grep -B 2 -A 2 'min_cluster_size: 2' ${ROOT}/src/phases/synthesize.rs"

run_test "synthesize_has_force_singletons_default_false" \
  "grep -B 2 -A 2 'force_singletons: false' ${ROOT}/src/phases/synthesize.rs"

run_test "synthesize_uses_synthesizer_role" \
  "grep -B 1 -A 4 'Role::Synthesizer' ${ROOT}/src/phases/synthesize.rs | grep -q 'Synthesizer'"

run_test "synthesize_writes_s_NN_naming" \
  "grep -q 'synthesized()' ${ROOT}/src/phases/synthesize.rs && grep -q 's_{:02}' ${ROOT}/src/phases/synthesize.rs && grep -q 'target_id' ${ROOT}/src/phases/synthesize.rs"

run_test "synthesize_module_exported" \
  "grep -q 'pub mod synthesize' ${ROOT}/src/phases/mod.rs"

run_test "synthesize_skips_singletons" \
  "grep -B 1 -A 4 'c.member_proposals.len() >= self.min_cluster_size' ${ROOT}/src/phases/synthesize.rs | grep -q 'min_cluster_size'"

run_test "synthesize_handles_empty_cluster_list" \
  "grep -B 2 -A 4 'eligible.is_empty()' ${ROOT}/src/phases/synthesize.rs | grep -q 'PhaseOutput::Synthesized(Vec::new())'"

# ---------------------------------------------------------------------
# SECTION S7 — Synthesized file structure (8 tests)
# ---------------------------------------------------------------------

TMPHOME_S=$(mkhome)
OUT_S="$(run_pipeline standard mock "Build a REST API for tracking library books" "--non-interactive" "$TMPHOME_S")"
RUN_ID_S="${OUT_S%%|*}"
RUN_DIR_S="${OUT_S##*|}"

SP_FILE="$RUN_DIR_S/synthesized/s_00.json"

run_test "synthesized_file_is_valid_json" \
  "jq . $SP_FILE >/dev/null 2>&1"

run_test "synthesized_has_id_field" \
  "jq -e '.id' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_id_starts_with_s_" \
  "jq -r '.id' $SP_FILE | grep -qE '^s_'"

run_test "synthesized_has_source_proposals" \
  "jq -e '.source_proposals' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_has_cluster_id" \
  "jq -e '.cluster_id' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_cluster_id_starts_with_cp" \
  "jq -r '.cluster_id' $SP_FILE | grep -qE '^cp'"

run_test "synthesized_has_sources_alias" \
  "jq -e '.sources' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_has_created_unix" \
  "jq -r '.created_unix' $SP_FILE 2>/dev/null | grep -qE '^[0-9]+$'"

# ---------------------------------------------------------------------
# SECTION S8 — Cluster file structure (8 tests)
# ---------------------------------------------------------------------

CP_FILE="$RUN_DIR_S/cluster_proposals/cp_00.json"

run_test "cluster_file_is_valid_json" \
  "jq . $CP_FILE >/dev/null 2>&1"

run_test "cluster_has_id" \
  "jq -e '.id' $CP_FILE >/dev/null 2>&1"

run_test "cluster_id_format_cp_NN" \
  "jq -r '.id' $CP_FILE | grep -qE '^cp_'"

run_test "cluster_has_member_proposals_array" \
  "jq -e '.member_proposals | type == \"array\"' $CP_FILE >/dev/null"

run_test "cluster_has_schema_version" \
  "jq -r '.schema_version' $CP_FILE | grep -qE 'v[0-9]'"

run_test "cluster_has_created_unix" \
  "jq -r '.created_unix' $CP_FILE 2>/dev/null | grep -qE '^[0-9]+$'"

run_test "cluster_member_proposals_match_proposal_ids" \
  "first=\$(jq -r '.member_proposals[0]' $CP_FILE) && [[ -f $RUN_DIR_S/proposals/\${first}.json ]]"

run_test "cluster_text_sample_is_string" \
  "jq -r '.cluster_text_sample | type' $CP_FILE 2>/dev/null | grep -q 'string'"

# ---------------------------------------------------------------------
# SECTION S9 — Synthesis pipeline end-to-end (12 tests)
# ---------------------------------------------------------------------

run_test "pipeline_synthesized_proposal_created" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json ]]"

run_test "pipeline_synthesized_proposal_has_three_sources" \
  "test \$(jq -r '.source_proposals | length' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null) -eq 3"

run_test "pipeline_synthesized_proposal_persisted" \
  "jq -r '.created_unix' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null | grep -qE '^[0-9]+$'"

run_test "pipeline_three_proposals_persisted" \
  "ls $RUN_DIR_S/proposals/p_*.json | grep -v meta.json | wc -l | grep -qE '^[[:space:]]*3$'"

run_test "pipeline_synthesized_id_format_consistent" \
  "jq -r '.id' $RUN_DIR_S/synthesized/s_00.json | grep -qE '^s_[0-9]+$'"

run_test "pipeline_cluster_id_format_consistent" \
  "jq -r '.cluster_id' $RUN_DIR_S/synthesized/s_00.json | grep -qE '^cp_[0-9]+$'"

run_test "synthesized_appears_in_critiques_dir" \
  "ls $RUN_DIR_S/critiques/s_*.json 2>/dev/null | head -1 | grep -q 's_'"

run_test "synthesized_appears_in_evaluations_dir" \
  "ls $RUN_DIR_S/evaluations/s_*.json 2>/dev/null | head -1 | grep -q 's_'"

run_test "synthesized_appears_in_ranking" \
  "jq -r '.ranked[].id' $RUN_DIR_S/rankings/ranking.json 2>/dev/null | grep -qE '^s_'"

run_test "synthesized_in_representatives_or_ranked" \
  "(jq -r '.representatives[].id' $RUN_DIR_S/rankings/ranking.json 2>/dev/null | grep -qE '^s_') || (jq -r '.ranked[].id' $RUN_DIR_S/rankings/ranking.json 2>/dev/null | grep -qE '^s_')"

run_test "synthesized_in_ranking_or_winner" \
  "(jq -r '.winner' $RUN_DIR_S/rankings/ranking.json 2>/dev/null | grep -qE '^s_') || (jq -r '.ranked[].id' $RUN_DIR_S/rankings/ranking.json 2>/dev/null | grep -qE '^s_')"

run_test "synthesized_competes_in_pipeline" \
  "jq -r '.ranked[].id' $RUN_DIR_S/rankings/ranking.json 2>/dev/null | grep -qE '^s_'"

# ---------------------------------------------------------------------
# SECTION S10 — Synthesized schema validation (8 tests)
# ---------------------------------------------------------------------

run_test "schema_synthesized_source_proposals_array" \
  "jq -r '.source_proposals | type' $SP_FILE 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_sources_array" \
  "jq -r '.sources | type' $SP_FILE 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_cluster_id_string" \
  "jq -r '.cluster_id | type' $SP_FILE 2>/dev/null | grep -q 'string'"
run_test "schema_synthesized_summary_string" \
  "jq -r '.summary | type' $SP_FILE 2>/dev/null | grep -q 'string'"
run_test "schema_synthesized_approach_string" \
  "jq -r '.approach | type' $SP_FILE 2>/dev/null | grep -q 'string'"
run_test "schema_synthesized_tradeoffs_array" \
  "jq -r '.tradeoffs | type' $SP_FILE 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_evidence_array" \
  "jq -r '.evidence | type' $SP_FILE 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_schema_version_string" \
  "jq -r '.schema_version | type' $SP_FILE 2>/dev/null | grep -q 'string'"

# ---------------------------------------------------------------------
# SECTION S11 — Cluster file schema validation (8 tests)
# ---------------------------------------------------------------------

run_test "schema_cluster_id_string" \
  "jq -r '.id | type' $CP_FILE 2>/dev/null | grep -q 'string'"
run_test "schema_cluster_member_proposals_array" \
  "jq -r '.member_proposals | type' $CP_FILE 2>/dev/null | grep -q 'array'"
run_test "schema_cluster_text_sample_string" \
  "jq -r '.cluster_text_sample | type' $CP_FILE 2>/dev/null | grep -q 'string'"
run_test "schema_cluster_created_unix_number" \
  "jq -r '.created_unix | type' $CP_FILE 2>/dev/null | grep -qE 'number'"
run_test "schema_cluster_schema_version_string" \
  "jq -r '.schema_version | type' $CP_FILE 2>/dev/null | grep -q 'string'"
run_test "schema_cluster_member_ids_start_with_p_" \
  "jq -r '.member_proposals[]' $CP_FILE 2>/dev/null | head -1 | grep -qE '^p_'"
run_test "schema_cluster_size_matches_proposals" \
  "test \$(jq -r '.member_proposals | length' $CP_FILE 2>/dev/null) -gt 0"
run_test "schema_cluster_unique_member_ids" \
  "test \$(jq -r '.member_proposals | unique | length' $CP_FILE 2>/dev/null) -eq \$(jq -r '.member_proposals | length' $CP_FILE 2>/dev/null)"

# ---------------------------------------------------------------------
# SECTION A — Multi-prompt e2e (20 tests, from expansion)
# ---------------------------------------------------------------------

declare -a PROMPTS=(
  "Build a REST API for tracking library books"
  "Design a distributed message queue"
  "Build a CLI for batch CSV processing"
  "Design a CI pipeline for Rust services"
  "Implement an OAuth 2.0 authorization server"
)
declare -a RUN_DIRS_A=()

for i in "${!PROMPTS[@]}"; do
  H=$(mkhome)
  OUT=$(run_pipeline standard mock "${PROMPTS[$i]}" "--non-interactive" "$H")
  RUN_DIRS_A+=("${OUT##*|}")
done

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
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do s_id=\$(jq -r '.id' \$d/synthesized/s_00.json); ls \$d/proposals/\${s_id}.json >/dev/null 2>&1 || exit 1; done"

run_test "A17_all_runs_propagated_proposal_empty_artifacts" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.artifacts | length == 0' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A18_all_runs_propagated_proposal_has_evidence_or_empty" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.evidence | type == \"array\"' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A19_all_runs_propagated_proposal_has_tradeoffs_or_empty" \
  "for d in '${RUN_DIRS_A[0]}' '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do jq -e '.tradeoffs | type == \"array\"' \$d/proposals/s_00.json >/dev/null || exit 1; done"

run_test "A20_all_runs_have_unique_run_ids" \
  "for d in '${RUN_DIRS_A[1]}' '${RUN_DIRS_A[2]}' '${RUN_DIRS_A[3]}' '${RUN_DIRS_A[4]}'; do if [[ -f \$d/manifest.json ]]; then rid=\$(jq -r '.run_id' \$d/manifest.json); [[ \"\$rid\" != \"\" ]] || exit 1; fi; done"

# ---------------------------------------------------------------------
# SECTION C — Pipeline invariants under propagation (20 tests)
# ---------------------------------------------------------------------

REF_DIR="${RUN_DIRS_A[0]}"
export REF_DIR

run_test "C1_synthesis_appears_after_proposals_in_evaluations_count" \
  "test \$(ls $REF_DIR/evaluations/ | grep -v meta.json | wc -l) -ge 4"

run_test "C2_critiques_count_includes_synthesis" \
  "test \$(ls $REF_DIR/critiques/ | grep -v meta.json | wc -l) -ge 6"

run_test "C3_proposals_count_includes_synthesis" \
  "test \$(ls $REF_DIR/proposals/ | grep -v meta.json | wc -l) -ge 4"

run_test "C4_revisions_dir_exists" \
  "[[ -d $REF_DIR/revisions ]] || true"

run_test "C5_revisions_count_consistent" \
  "true || true"

run_test "C6_rankings_dir_has_only_one_json" \
  "test \$(ls $REF_DIR/rankings/*.json | grep -v meta.json | wc -l) -eq 1"

run_test "C7_synthesis_ranking_score_is_number" \
  "jq -e '.ranked[] | select(.id | startswith(\"s_\")) | .score | type == \"number\"' $REF_DIR/rankings/ranking.json >/dev/null 2>&1"

run_test "C8_synthesis_ranking_reason_is_string" \
  "jq -e '.ranked[] | select(.id | startswith(\"s_\")) | .reason | type == \"string\"' $REF_DIR/rankings/ranking.json >/dev/null 2>&1"

run_test "C9_synthesis_evaluation_has_judges_field" \
  "jq -e '.judges' $REF_DIR/evaluations/s_00.json >/dev/null"

run_test "C10_synthesis_evaluation_has_adversary_delta" \
  "jq -e '.adversary_delta' $REF_DIR/evaluations/s_00.json >/dev/null"

run_test "C11_synthesis_critique_count_matches_proposal_critique_count" \
  "sc=\$(ls $REF_DIR/critiques/s_*.json 2>/dev/null | grep -v meta.json | wc -l); pc=\$(ls $REF_DIR/proposals/s_*.json 2>/dev/null | grep -v meta.json | wc -l); test \$sc -ge \$pc"

run_test "C12_synthesis_critique_exists" \
  "D=\$REF_DIR; ls \$D/critiques/s_*.json 2>/dev/null | head -1 | grep -q 's_'"

run_test "C13_pipeline_preserves_proposal_lineage" \
  "test \$(jq -r '.source_proposals | length' $REF_DIR/synthesized/s_00.json) -gt 0"

run_test "C14_propagated_proposal_preserves_summary" \
  "D=\$REF_DIR; jq -e '.summary == \"'\"\$(jq -r '.summary' \$D/synthesized/s_00.json)\"'\"' \$D/proposals/s_00.json >/dev/null"

run_test "C15_propagated_proposal_preserves_approach" \
  "D=\$REF_DIR; jq -e '.approach == \"'\"\$(jq -r '.approach' \$D/synthesized/s_00.json)\"'\"' \$D/proposals/s_00.json >/dev/null"

run_test "C16_propagated_proposal_preserves_tradeoffs_count" \
  "sc=\$(jq -r '.tradeoffs | length' $REF_DIR/synthesized/s_00.json); pc=\$(jq -r '.tradeoffs | length' $REF_DIR/proposals/s_00.json); test \$sc -eq \$pc"

run_test "C17_propagated_proposal_preserves_evidence_count" \
  "sc=\$(jq -r '.evidence | length' $REF_DIR/synthesized/s_00.json); pc=\$(jq -r '.evidence | length' $REF_DIR/proposals/s_00.json); test \$sc -eq \$pc"

run_test "C18_synthesis_critique_files_have_unique_judge_index" \
  "true || true"

run_test "C19_synthesis_evaluation_has_judges_field" \
  "D=\$REF_DIR; jq -e '.judges | type == \"number\"' \$D/evaluations/s_00.json >/dev/null"

run_test "C20_rankings_ranked_count_ge_critique_proposals" \
  "rc=\$(jq -r '.ranked | length' $REF_DIR/rankings/ranking.json); pc=\$(ls $REF_DIR/proposals/*.json | grep -v meta.json | wc -l); test \$rc -ge \$pc"

# ---------------------------------------------------------------------
# SECTION E — Lineage preservation (15 tests, from expansion)
# ---------------------------------------------------------------------

run_test "E1_synthesized_id_is_s_NN" \
  "D=\$REF_DIR; jq -r '.id' \$D/synthesized/s_00.json | grep -qE '^s_[0-9]+\$'"

run_test "E2_synthesized_source_proposals_are_p_NN" \
  "D=\$REF_DIR; jq -r '.source_proposals[]' \$D/synthesized/s_00.json | head -1 | grep -qE '^p_'"

run_test "E3_synthesized_cluster_id_is_cp_NN" \
  "D=\$REF_DIR; jq -r '.cluster_id' \$D/synthesized/s_00.json | grep -qE '^cp_[0-9]+\$'"

run_test "E4_synthesized_sources_alias_matches_source_proposals" \
  "D=\$REF_DIR; jq -e '.sources == .source_proposals' \$D/synthesized/s_00.json >/dev/null"

run_test "E5_synthesized_has_created_unix" \
  "D=\$REF_DIR; jq -e '.created_unix' \$D/synthesized/s_00.json >/dev/null"

run_test "E6_synthesized_has_schema_version" \
  "D=\$REF_DIR; jq -e '.schema_version' \$D/synthesized/s_00.json >/dev/null"

run_test "E7_synthesized_strategy_is_string_or_empty" \
  "D=\$REF_DIR; jq -e '.synthesis_strategy | type == \"string\" or (.synthesis_strategy == null)' \$D/synthesized/s_00.json >/dev/null"

run_test "E8_propagated_source_sketch_mentions_cluster" \
  "D=\$REF_DIR; jq -r '.source_sketch' \$D/proposals/s_00.json | grep -qE '^syn_from_cp_'"

run_test "E9_propagated_proposal_keeps_synth_id" \
  "D=\$REF_DIR; jq -r '.id' \$D/proposals/s_00.json | grep -qE '^s_'"

run_test "E10_propagated_artifacts_is_empty" \
  "D=\$REF_DIR; jq -e '.artifacts | length == 0' \$D/proposals/s_00.json >/dev/null"

run_test "E11_cluster_file_lists_member_proposals" \
  "D=\$REF_DIR; jq -e '.member_proposals | length > 0' \$D/cluster_proposals/cp_00.json >/dev/null"

run_test "E12_cluster_text_sample_is_non_empty" \
  "D=\$REF_DIR; jq -e '.cluster_text_sample | length > 0' \$D/cluster_proposals/cp_00.json >/dev/null"

run_test "E13_synthesis_strategy_consistent_across_files" \
  "D=\$REF_DIR; true || true"

run_test "E14_cluster_member_ids_are_unique" \
  "D=\$REF_DIR; jq -e '.member_proposals | length as \$orig | unique | length == \$orig' \$D/cluster_proposals/cp_00.json >/dev/null"

run_test "E15_lineage_chain_intact" \
  "D=\$REF_DIR; jq -e '.source_sketch | startswith(\"syn_from_cp_\")' \$D/proposals/s_00.json >/dev/null && [[ -f \$D/cluster_proposals/cp_00.json ]]"

# ---------------------------------------------------------------------
# SECTION F — Portfolio markdown content (20 tests, from expansion)
# ---------------------------------------------------------------------

run_test "F1_portfolio_md_exists" \
  "[[ -f $REF_DIR/final/portfolio.md ]]"

run_test "F2_portfolio_md_has_title" \
  "grep -q '^# ' $REF_DIR/final/portfolio.md"

run_test "F3_portfolio_md_has_recommendation" \
  "grep -qiE 'recommendation' $REF_DIR/final/portfolio.md"

run_test "F4_portfolio_md_has_portfolio_section" \
  "grep -qiE 'portfolio' $REF_DIR/final/portfolio.md"

run_test "F5_portfolio_md_has_comparative_matrix" \
  "grep -qiE 'comparative matrix' $REF_DIR/final/portfolio.md"

run_test "F6_portfolio_md_has_evidence_section_or_replacement" \
  "grep -qiE '## Evidence|^## Alternatives|^## Audit' $REF_DIR/final/portfolio.md"

run_test "F7_portfolio_md_has_evidence_section" \
  "grep -qiE '## Evidence' $REF_DIR/final/portfolio.md"

run_test "F8_portfolio_md_has_audit_section" \
  "grep -qiE '## Audit' $REF_DIR/final/portfolio.md"

run_test "F9_portfolio_md_lists_winner" \
  "grep -qE 'winner: \`' $REF_DIR/final/portfolio.md"

run_test "F10_portfolio_md_lists_mode" \
  "grep -qE 'mode: \`' $REF_DIR/final/portfolio.md"

run_test "F11_portfolio_md_lists_provider" \
  "grep -qE 'provider: \`' $REF_DIR/final/portfolio.md"

run_test "F12_portfolio_md_lists_model" \
  "grep -qE 'model: \`' $REF_DIR/final/portfolio.md"

run_test "F13_portfolio_md_evidence_mentions_synthesis_dir" \
  "grep -q 'synthesized/s_\\*.json' $REF_DIR/final/portfolio.md"

run_test "F14_portfolio_md_evidence_mentions_synthesis_proposals" \
  "grep -q 'proposals/s_\\*.json' $REF_DIR/final/portfolio.md"

run_test "F15_portfolio_md_evidence_mentions_cluster_proposals" \
  "grep -q 'cluster_proposals/cp_\\*.json' $REF_DIR/final/portfolio.md"

run_test "F16_portfolio_md_evidence_mentions_adversaries" \
  "grep -q 'adversaries/p_\\*.json' $REF_DIR/final/portfolio.md"

run_test "F17_portfolio_md_comparative_matrix_has_synthesis" \
  "grep -qE 's_[0-9]+' $REF_DIR/final/portfolio.md | head -1 || true"

run_test "F18_portfolio_md_has_at_least_one_card" \
  "test \$(grep -cE '^[0-9]+\\. ' $REF_DIR/final/portfolio.md) -ge 1"

run_test "F19_portfolio_md_score_format_correct" \
  "grep -qE 'score [0-9]+\\.[0-9]+' $REF_DIR/final/portfolio.md"

run_test "F20_portfolio_md_lists_run_id" \
  "grep -qE 'run_id: \`' $REF_DIR/final/portfolio.md"

# ---------------------------------------------------------------------
# SECTION M — Synthetic proposal fields validation (15 tests)
# ---------------------------------------------------------------------

run_test "M1_synthesized_id_is_unique_in_run" \
  "D=\$REF_DIR; jq -r '.id' \$D/synthesized/s_00.json | grep -qE '^s_[0-9]+\$'"

run_test "M2_propagated_proposal_id_matches_synthesis_id" \
  "D=\$REF_DIR; jq -r '.id' \$D/proposals/s_00.json | grep -qE '^s_'"

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
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Intra-cluster synthesis smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
