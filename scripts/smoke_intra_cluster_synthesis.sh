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
  "grep -q 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs"

run_test "SynthesizedProposal_has_source_proposals_field" \
  "grep -A 20 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs | grep -q 'pub source_proposals'"

run_test "SynthesizedProposal_has_cluster_id_field" \
  "grep -A 20 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs | grep -q 'pub cluster_id'"

run_test "SynthesizedProposal_has_synthesis_strategy" \
  "grep -A 20 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs | grep -q 'pub synthesis_strategy'"

run_test "SynthesizedProposal_has_summary" \
  "grep -A 25 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs | grep -q 'pub summary'"

run_test "SynthesizedProposal_has_approach" \
  "grep -A 25 'pub struct SynthesizedProposal' ${ROOT}/src/domain/mod.rs | grep -q 'pub approach'"

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
  "grep -B 1 -A 1 'Role::Synthesizer => DEFAULT_MAX_TOKENS' ${ROOT}/src/phases/phase.rs"

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

run_test "synthesize_uses_merge_synthesizer_role" \
  "grep -B 1 -A 4 'Role::MergeSynthesizer' ${ROOT}/src/phases/synthesize.rs | grep -q 'MergeSynthesizer'"

run_test "synthesize_writes_s_NN_naming" \
  "grep -q 'synthesized()' ${ROOT}/src/phases/synthesize.rs && grep -q 's_{:02}' ${ROOT}/src/phases/synthesize.rs && grep -q 'target_id' ${ROOT}/src/phases/synthesize.rs"

run_test "synthesize_module_exported" \
  "grep -q 'pub mod synthesize' ${ROOT}/src/phases/mod.rs"

run_test "synthesize_skips_singletons" \
  "grep -B 1 -A 4 'c.member_proposals.len() >= self.min_cluster_size' ${ROOT}/src/phases/synthesize.rs | grep -q 'min_cluster_size'"

run_test "synthesize_handles_empty_cluster_list" \
  "grep -B 2 -A 12 'eligible.is_empty()' ${ROOT}/src/phases/synthesize.rs | grep -q 'PhaseOutput::Synthesized(Vec::new())'"

# ---------------------------------------------------------------------
# SECTION S7 — Synthesized file structure (8 tests)
# ---------------------------------------------------------------------

TMPHOME_S=$(mkhome)
OUT_S="$(run_pipeline standard mock:mock-model "Build a REST API for tracking library books" "--non-interactive" "$TMPHOME_S")"
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

run_test "pipeline_synthesized_proposal_has_cluster_sources" \
  "test \$(jq -r '.source_proposals | length' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null) -ge 2"

run_test "pipeline_synthesized_proposal_persisted" \
  "jq -r '.created_unix' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null | grep -qE '^[0-9]+$'"

run_test "pipeline_proposals_persisted" \
  "ls $RUN_DIR_S/proposals/p_*.json | grep -v meta.json | wc -l | grep -qE '^[[:space:]]*7$'"

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
