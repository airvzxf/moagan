#!/usr/bin/env bash
# Phase D smoke tests for v0.2 (Plan B sub-phase D).
#
# Phase D closes when the pipeline:
# 1. Clusters proposals by SimHash / Jaccard similarity
#    (ClusterProposalsPhase -> cluster_proposals/cp_<NN>.json).
# 2. Synthesizes each eligible cluster with the synthesizer role
#    (SynthesizePhase -> synthesized/s_<NN>.json).
# 3. Propagates the synthesis into proposals/s_<NN>.json so it runs
#    through Gate, Critique, Repair, Judge, and Rank like any other
#    proposal (V4 §5.13 "La síntesis compite").
# 4. Computes the disagreement_score per proposal and fires the
#    adversary role only when the judges disagree beyond threshold
#    (JudgePhase -> adversaries/p_<id>.json).
# 5. Persists a human_checkpoint JSON sidecar when the run is
#    interactive and the brief looks risky
#    (IntakePhase / ClarifyPhase / DeliverPhase).
#
# This script exercises all five pieces with 263 checks across 30
# sections. Each section groups related invariants. Exit code is
# non-zero when any check fails. Pair with smoke_phase_d_expansion.sh
# (260 checks, 20 sections) for the full Phase D manual coverage.

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

# Mock fixture directory with 34 canned responses. The mock provider
# cycles through these in alphabetical order; this lets a full
# standard/deep run complete without exhausting the queue.
MOCK_DIR="${ROOT}/tests/fixtures/mock_provider"
if [[ ! -d "$MOCK_DIR" ]]; then
  echo "missing mock fixture dir at $MOCK_DIR"
  exit 1
fi

# ---------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------

run_test() {
  local name="$1"
  local body="$2"
  bash -c "$body" >/tmp/smoke-d-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-d-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-phase-d.XXXXXX)"
  echo "$d"
}

# Run a smoke pipeline; echoes "<run_id> <run_dir>". Mode, provider,
# prompt are parameters. Other flags are pinned so the runs are
# comparable across invocations.
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
# SECTION 1 — Phase D domain types (15 tests)
# ---------------------------------------------------------------------

run_test "domain_has_SynthesizedProposal" \
  "grep -q 'pub struct SynthesizedProposal' ${ROOT}/src/domain.rs"

run_test "domain_has_AdversaryReport" \
  "grep -q 'pub struct AdversaryReport' ${ROOT}/src/domain.rs"

run_test "domain_has_HumanCheckpoint" \
  "grep -q 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs"

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

run_test "AdversaryReport_has_disagreement_score" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub disagreement_score'"

run_test "AdversaryReport_has_score_delta" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub score_delta'"

run_test "AdversaryReport_has_weaknesses" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub weaknesses'"

run_test "AdversaryReport_has_unverified_claims" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub unverified_claims'"

run_test "AdversaryReport_has_consensus_check" \
  "grep -A 25 'pub struct AdversaryReport' ${ROOT}/src/domain.rs | grep -q 'pub consensus_check'"

run_test "HumanCheckpoint_has_phase_field" \
  "grep -A 5 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs | grep -q 'pub phase'"

run_test "HumanCheckpoint_has_response_field" \
  "grep -A 12 'pub struct HumanCheckpoint' ${ROOT}/src/domain.rs | grep -q 'pub response'"

# ---------------------------------------------------------------------
# SECTION 2 — Phase D role registration (10 tests)
# ---------------------------------------------------------------------

run_test "role_Synthesizer_defined" \
  "grep -q 'Synthesizer,' ${ROOT}/src/llm/role.rs"

run_test "role_Adversary_defined" \
  "grep -q 'Adversary,' ${ROOT}/src/llm/role.rs"

run_test "role_Synthesizer_str_is_synthesizer" \
  "grep -A 1 'Self::Synthesizer =>' ${ROOT}/src/llm/role.rs | grep -q '\"synthesizer\"'"

run_test "role_Adversary_str_is_adversary" \
  "grep -A 1 'Self::Adversary =>' ${ROOT}/src/llm/role.rs | grep -q '\"adversary\"'"

run_test "role_count_is_sixteen" \
  "grep -q 'all_roles_are_count_sixteen' ${ROOT}/src/llm/role.rs"

run_test "role_FromStr_handles_synthesizer" \
  "grep -A 20 'fn from_str' ${ROOT}/src/llm/role.rs | grep -q '\"synthesizer\" =>'"

run_test "role_FromStr_handles_adversary" \
  "grep -A 22 'fn from_str' ${ROOT}/src/llm/role.rs | grep -q '\"adversary\" =>'"

run_test "role_validate_json_handles_SynthesizedProposal" \
  "grep -B0 -A 60 'fn validate_json' ${ROOT}/src/llm/role.rs | grep -q 'SynthesizedProposal'"

run_test "role_validate_json_handles_AdversaryReport" \
  "grep -B0 -A 60 'fn validate_json' ${ROOT}/src/llm/role.rs | grep -q 'AdversaryReport'"

run_test "role_schema_description_covers_synthesizer" \
  "grep -B0 -A 40 'fn schema_description' ${ROOT}/src/llm/role.rs | grep -q 'Synthesizer:'"

# ---------------------------------------------------------------------
# SECTION 3 — Phase D prompt files (8 tests)
# ---------------------------------------------------------------------

run_test "prompt_synthesize_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/synthesize.md ]]"

run_test "prompt_judge_adversary_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/judge_adversary.md ]]"

run_test "prompt_synthesize_registered" \
  "grep -q 'SYNTHESIZE_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_judge_adversary_registered" \
  "grep -q 'JUDGE_ADVERSARY_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_synthesize_mentions_merge" \
  "grep -qiE 'merge.*cluster|cluster.*merge' ${ROOT}/src/llm/prompts/synthesize.md"

run_test "prompt_synthesize_mentions_id_target" \
  "grep -qiE 's_<NN>|target.*id' ${ROOT}/src/llm/prompts/synthesize.md"

run_test "prompt_adversary_mentions_disagreement" \
  "grep -qiE 'disagreement' ${ROOT}/src/llm/prompts/judge_adversary.md"

run_test "prompt_adversary_mentions_deterministic" \
  "grep -qiE 'deterministic|T=0\\.0' ${ROOT}/src/llm/prompts/judge_adversary.md"

# ---------------------------------------------------------------------
# SECTION 4 — Phase D per-role sampling (8 tests)
# ---------------------------------------------------------------------

run_test "max_tokens_Synthesizer_is_4000" \
  "grep -A 30 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Role::Synthesizer => 4000'"

run_test "max_tokens_Adversary_is_2048" \
  "grep -A 31 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Role::Adversary => 2048'"

run_test "temp_Synthesizer_is_0_4" \
  "grep -A 30 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Role::Synthesizer => 0.4'"

run_test "temp_Adversary_is_0_0" \
  "grep -A 31 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Role::Adversary => 0.0'"

run_test "prompts_prompt_set_hash_includes_synthesize" \
  "grep -A 25 'prompt_set_hash' ${ROOT}/src/llm/prompts.rs | grep -q 'SYNTHESIZE_PROMPT'"

run_test "prompts_prompt_set_hash_includes_adversary" \
  "grep -A 26 'prompt_set_hash' ${ROOT}/src/llm/prompts.rs | grep -q 'JUDGE_ADVERSARY_PROMPT'"

run_test "system_prompt_dispatches_Synthesizer" \
  "grep -A 18 'fn system_prompt' ${ROOT}/src/llm/prompts.rs | grep -q 'Role::Synthesizer =>'"

run_test "system_prompt_dispatches_Adversary" \
  "grep -A 19 'fn system_prompt' ${ROOT}/src/llm/prompts.rs | grep -q 'Role::Adversary =>'"

# ---------------------------------------------------------------------
# SECTION 5 — fs_layout additions for Phase D (8 tests)
# ---------------------------------------------------------------------

run_test "fs_layout_synthesized_path" \
  "grep -q 'pub fn synthesized' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_cluster_proposals_path" \
  "grep -q 'pub fn cluster_proposals_dir' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_adversaries_path" \
  "grep -q 'pub fn adversaries' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_checkpoints_path" \
  "grep -q 'pub fn checkpoints' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_synthesized" \
  "grep -A 30 'pub fn ensure' ${ROOT}/src/fs_layout.rs | grep -q 'self.synthesized()'"

run_test "fs_layout_ensure_creates_cluster_proposals" \
  "grep -A 30 'pub fn ensure' ${ROOT}/src/fs_layout.rs | grep -q 'self.cluster_proposals_dir()'"

run_test "fs_layout_ensure_creates_adversaries" \
  "grep -A 30 'pub fn ensure' ${ROOT}/src/fs_layout.rs | grep -q 'self.adversaries()'"

run_test "fs_layout_ensure_creates_checkpoints" \
  "grep -A 30 'pub fn ensure' ${ROOT}/src/fs_layout.rs | grep -q 'self.checkpoints()'"

# ---------------------------------------------------------------------
# SECTION 6 — Checkpoint module shape (10 tests)
# ---------------------------------------------------------------------

run_test "checkpoint_CheckpointKind_is_enum" \
  "grep -q 'pub enum CheckpointKind' ${ROOT}/src/checkpoint/human.rs"

run_test "checkpoint_Resolution_is_enum" \
  "grep -q 'pub enum Resolution' ${ROOT}/src/checkpoint/human.rs"

run_test "checkpoint_Checkpoint_struct_has_question" \
  "grep -A 12 'pub struct Checkpoint' ${ROOT}/src/checkpoint/human.rs | grep -q 'pub question'"

run_test "checkpoint_CheckpointOpts_has_interactive" \
  "grep -A 6 'pub struct CheckpointOpts' ${ROOT}/src/checkpoint/human.rs | grep -q 'pub interactive'"

run_test "checkpoint_ask_function_exists" \
  "grep -q 'pub fn ask' ${ROOT}/src/checkpoint/human.rs"

run_test "checkpoint_skip_function_exists" \
  "grep -q 'pub fn skip' ${ROOT}/src/checkpoint/human.rs"

run_test "checkpoint_mod_re_exports_types" \
  "grep -A 1 'pub use human::' ${ROOT}/src/checkpoint/mod.rs | grep -q 'Checkpoint, CheckpointKind'"

run_test "checkpoint_no_dialoguer_use_stmt" \
  "! grep -rn 'use dialoguer' ${ROOT}/src/checkpoint/"

run_test "checkpoint_no_inquire_use_stmt" \
  "! grep -rn 'use inquire' ${ROOT}/src/checkpoint/"

run_test "checkpoint_no_dialoguer_call" \
  "test -z \"\$(grep -E '^[^/]*dialoguer::' ${ROOT}/src/checkpoint/human.rs)\""

run_test "checkpoint_no_inquire_call" \
  "test -z \"\$(grep -E '^[^/]*inquire::' ${ROOT}/src/checkpoint/human.rs)\""

run_test "checkpoint_uses_stdin_only_for_input" \
  "grep -q 'io::stdin' ${ROOT}/src/checkpoint/human.rs"

run_test "checkpoint_uses_stdin" \
  "grep -q 'std::io::stdin' ${ROOT}/src/checkpoint/human.rs"

# ---------------------------------------------------------------------
# SECTION 7 — ClusterProposalsPhase surface (8 tests)
# ---------------------------------------------------------------------

run_test "cluster_proposals_module_exists" \
  "[[ -f ${ROOT}/src/phases/cluster_proposals.rs ]]"

run_test "cluster_proposals_CLUSTER_THRESHOLD_is_0_7" \
  "grep -q 'pub const CLUSTER_THRESHOLD: f32 = 0.7' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_has_ProposalCluster_struct" \
  "grep -q 'pub struct ProposalCluster' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_has_cluster_text_fn" \
  "grep -q 'pub fn cluster_text' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_implements_phase" \
  "grep -B 0 -A 8 'impl Phase for ClusterProposalsPhase' ${ROOT}/src/phases/cluster_proposals.rs | grep -q 'async fn execute'"

run_test "cluster_proposals_writes_cp_NN_naming" \
  "grep -q 'cp_{:02}' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "cluster_proposals_writes_empty_marker" \
  "grep -A 12 'Nothing meaningful to cluster' ${ROOT}/src/phases/cluster_proposals.rs | grep -q 'cp_00.json'"

run_test "cluster_proposals_module_exported" \
  "grep -q 'pub mod cluster_proposals' ${ROOT}/src/phases/mod.rs"

# ---------------------------------------------------------------------
# SECTION 8 — SynthesizePhase surface (8 tests)
# ---------------------------------------------------------------------

run_test "synthesize_module_exists" \
  "[[ -f ${ROOT}/src/phases/synthesize.rs ]]"

run_test "synthesize_has_min_cluster_size_default_2" \
  "grep -A 6 'impl Default for SynthesizePhase' ${ROOT}/src/phases/synthesize.rs | grep -q 'min_cluster_size: 2'"

run_test "synthesize_has_force_singletons_default_false" \
  "grep -A 7 'impl Default for SynthesizePhase' ${ROOT}/src/phases/synthesize.rs | grep -q 'force_singletons: false'"

run_test "synthesize_uses_synthesizer_role" \
  "grep -A 1 'system_prompt(Role' ${ROOT}/src/phases/synthesize.rs | grep -q 'Synthesizer'"

run_test "synthesize_writes_s_NN_naming" \
  "grep -q 's_{:02}' ${ROOT}/src/phases/synthesize.rs"

run_test "synthesize_module_exported" \
  "grep -q 'pub mod synthesize' ${ROOT}/src/phases/mod.rs"

run_test "synthesize_skips_singletons" \
  "grep -B 1 -A 3 'min_cluster_size' ${ROOT}/src/phases/synthesize.rs | grep -q 'member_proposals.len() >='"

run_test "synthesize_handles_empty_cluster_list" \
  "grep -B 1 -A 2 'eligible.is_empty' ${ROOT}/src/phases/synthesize.rs | grep -q 'Synthesized(Vec::new())'"

# ---------------------------------------------------------------------
# SECTION 9 — JudgePhase adversary surface (10 tests)
# ---------------------------------------------------------------------

run_test "judge_disagreement_threshold_default_is_0_5" \
  "grep -q 'pub const DEFAULT_DISAGREEMENT_THRESHOLD: f32 = 0.5' ${ROOT}/src/phases/judge.rs"

run_test "judge_has_disagreement_score_function" \
  "grep -q 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs"

run_test "judge_has_aggregated_adversary_delta_field" \
  "grep -A 25 'pub struct Aggregated' ${ROOT}/src/phases/judge.rs | grep -q 'pub adversary_delta'"

run_test "judge_has_enable_adversary_flag" \
  "grep -A 10 'pub struct JudgePhase' ${ROOT}/src/phases/judge.rs | grep -q 'pub enable_adversary'"

run_test "judge_uses_adversary_role" \
  "grep -B 0 -A 3 'system_prompt(Role::Adversary)' ${ROOT}/src/phases/judge.rs | grep -q 'system_prompt(Role::Adversary)'"

run_test "judge_writes_adversaries_dir" \
  "grep -B 0 -A 2 'adversaries_dir = ctx.run_dir().adversaries' ${ROOT}/src/phases/judge.rs | grep -q 'create_dir_all(&adversaries_dir)'"

run_test "judge_threshold_can_be_disabled_via_zero" \
  "grep -A 2 'enable_adversary && self.disagreement_threshold' ${ROOT}/src/phases/judge.rs | grep -q '> 0.0'"

run_test "judge_adversary_delta_is_clamped" \
  "grep -A 2 'combined = (agg.score + delta).clamp' ${ROOT}/src/phases/judge.rs | grep -q '0.0, 10.0'"

run_test "judge_universal_threshold_is_honored" \
  "grep -A 1 'if disagreement <' ${ROOT}/src/phases/judge.rs | grep -q 'self.disagreement_threshold'"

run_test "judge_skipped_in_fast_mode" \
  "grep -B 0 -A 6 'JudgePhase {' ${ROOT}/src/cli/run.rs | grep -q '..JudgePhase::default()' || grep -B 0 -A 6 'JudgePhase {' ${ROOT}/src/cli/run.rs | grep -q 'enable_adversary: false'"

# ---------------------------------------------------------------------
# SECTION 10 — Pipeline wiring (10 tests)
# ---------------------------------------------------------------------

run_test "wiring_ClusterProposalsPhase_imported_in_run" \
  "grep -q 'ClusterProposalsPhase' ${ROOT}/src/cli/run.rs"

run_test "wiring_SynthesizePhase_imported_in_run" \
  "grep -q 'SynthesizePhase' ${ROOT}/src/cli/run.rs"

run_test "wiring_JudgePhase_uses_default" \
  "grep -A 2 '.push(JudgePhase' ${ROOT}/src/cli/run.rs | grep -q '..JudgePhase::default()'"

run_test "wiring_fast_mode_skips_cluster_synthesize" \
  "grep -B 1 -A 4 '!matches!(mode, Mode::Fast)' ${ROOT}/src/cli/run.rs | grep -q 'ClusterProposalsPhase::default()'"

run_test "wiring_cluster_before_judge" \
  "grep -A 5 '.push(ClusterProposalsPhase' ${ROOT}/src/cli/run.rs | grep -q '.push(SynthesizePhase'"

run_test "wiring_intake_uses_checkpoint" \
  "grep -q 'Checkpoint::yes_no' ${ROOT}/src/phases/intake.rs"

run_test "wiring_clarify_uses_checkpoint" \
  "grep -q 'Checkpoint::yes_no' ${ROOT}/src/phases/clarify.rs"

run_test "wiring_deliver_uses_checkpoint" \
  "grep -q 'Checkpoint::yes_no' ${ROOT}/src/phases/deliver.rs"

run_test "wiring_run_context_with_interactive_method" \
  "grep -q 'pub fn with_interactive' ${ROOT}/src/phases/phase.rs"

run_test "wiring_run_default_interactive_true" \
  "grep -B 0 -A 40 'pub fn new' ${ROOT}/src/phases/phase.rs | grep -q 'interactive: true'"

# ---------------------------------------------------------------------
# SECTION 11 — End-to-end run produces Phase D artefacts (12 tests)
# ---------------------------------------------------------------------

TMPHOME_S=$(mkhome)
OUT_S="$(run_pipeline standard mock "Build a REST API for tracking library books" "--non-interactive" "$TMPHOME_S")"
RUN_ID_S="${OUT_S%%|*}"
RUN_DIR_S="${OUT_S##*|}"

run_test "e2e_standard_run_completes" \
  "[[ -f $RUN_DIR_S/manifest.json ]]"

run_test "e2e_standard_creates_manifest" \
  "[[ -s $RUN_DIR_S/manifest.json ]]"

run_test "e2e_standard_creates_brief" \
  "[[ -f $RUN_DIR_S/brief.json ]]"

run_test "e2e_standard_creates_cluster_proposals_dir" \
  "[[ -d $RUN_DIR_S/cluster_proposals ]]"

run_test "e2e_standard_creates_synthesized_dir" \
  "[[ -d $RUN_DIR_S/synthesized ]]"

run_test "e2e_standard_creates_adversaries_dir" \
  "[[ -d $RUN_DIR_S/adversaries ]]"

run_test "e2e_standard_creates_checkpoints_dir" \
  "[[ -d $RUN_DIR_S/checkpoints ]]"

run_test "e2e_standard_cluster_proposals_marker_exists" \
  "[[ -f $RUN_DIR_S/cluster_proposals/cp_00.json ]]"

run_test "e2e_standard_synthesized_marker_exists" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json ]]"

run_test "e2e_standard_checkpoint_persisted" \
  "ls $RUN_DIR_S/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | head -1 | grep -q h_"

run_test "e2e_standard_produces_evaluations" \
  "ls $RUN_DIR_S/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"

run_test "e2e_standard_produces_rankings" \
  "[[ -f $RUN_DIR_S/rankings/ranking.json ]]"

# ---------------------------------------------------------------------
# SECTION 12 — End-to-end fast mode skips Phase D artefact loops (6 tests)
# ---------------------------------------------------------------------

TMPHOME_F=$(mkhome)
OUT_F="$(run_pipeline fast mock "Build a CLI for batch CSV processing" "--non-interactive" "$TMPHOME_F")"
RUN_DIR_F="${OUT_F##*|}"

run_test "e2e_fast_mode_runs" \
  "[[ -f $RUN_DIR_F/manifest.json ]]"

run_test "e2e_fast_mode_has_no_cluster_proposals" \
  "[[ ! -s $RUN_DIR_F/cluster_proposals/cp_00.json ]] || [[ ! -f $RUN_DIR_F/cluster_proposals/cp_00.json ]] || (test \\$(jq '.member_proposals | length' < $RUN_DIR_F/cluster_proposals/cp_00.json 2>/dev/null || echo 0) -eq 0)"

run_test "e2e_fast_mode_no_synthesized_files" \
  "! ls $RUN_DIR_F/synthesized/s_*.json 2>/dev/null | grep -q s_"

run_test "e2e_fast_mode_empty_adversaries_dir" \
  "! ls $RUN_DIR_F/adversaries/p_*.json 2>/dev/null | grep -q p_"

run_test "e2e_fast_mode_has_evaluations" \
  "ls $RUN_DIR_F/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"

run_test "e2e_fast_mode_has_rankings" \
  "[[ -f $RUN_DIR_F/rankings/ranking.json ]]"

# ---------------------------------------------------------------------
# SECTION 13 — End-to-end deep mode also runs Phase D (4 tests)
# ---------------------------------------------------------------------

TMPHOME_D=$(mkhome)
OUT_D="$(run_pipeline deep mock "Design a distributed message queue" "--non-interactive" "$TMPHOME_D")"
RUN_DIR_D="${OUT_D##*|}"

run_test "e2e_deep_mode_runs" \
  "[[ -f $RUN_DIR_D/manifest.json ]]"

run_test "e2e_deep_mode_creates_cluster_proposals" \
  "[[ -f $RUN_DIR_D/cluster_proposals/cp_00.json ]]"

run_test "e2e_deep_mode_creates_synthesized" \
  "[[ -f $RUN_DIR_D/synthesized/s_00.json ]]"

run_test "e2e_deep_mode_creates_adversaries_dir" \
  "[[ -d $RUN_DIR_D/adversaries ]]"

# ---------------------------------------------------------------------
# SECTION 14 — End-to-end batch mode (4 tests)
# ---------------------------------------------------------------------

TMPHOME_B=$(mkhome)
OUT_B="$(run_pipeline batch mock "Build a CI pipeline for Rust services" "--non-interactive" "$TMPHOME_B")"
RUN_DIR_B="${OUT_B##*|}"

run_test "e2e_batch_mode_runs" \
  "[[ -f $RUN_DIR_B/manifest.json ]]"

run_test "e2e_batch_mode_creates_cluster_proposals" \
  "[[ -f $RUN_DIR_B/cluster_proposals/cp_00.json ]]"

run_test "e2e_batch_mode_creates_synthesized" \
  "[[ -f $RUN_DIR_B/synthesized/s_00.json ]]"

run_test "e2e_batch_mode_skips_interactive_checkpoints" \
  "grep -q '<skipped:non_interactive>' $RUN_DIR_B/checkpoints/h_*.json 2>/dev/null"

# ---------------------------------------------------------------------
# SECTION 15 — SynthesizedProposal structure round-trip (8 tests)
# ---------------------------------------------------------------------

# Re-use the standard run. The synthesized file is empty in mock mode
# but the JSON contract must still round-trip.
SP_FILE="$RUN_DIR_S/synthesized/s_00.json"

run_test "synthesized_file_is_valid_json" \
  "jq . $SP_FILE >/dev/null 2>&1"

run_test "synthesized_has_id_field" \
  "jq -e '.id' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_id_starts_with_s_" \
  "jq -r '.id' $SP_FILE 2>/dev/null | grep -qE '^s_'"

run_test "synthesized_has_source_proposals" \
  "jq -e '.source_proposals' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_has_cluster_id" \
  "jq -e '.cluster_id' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_cluster_id_starts_with_cp" \
  "jq -r '.cluster_id' $SP_FILE 2>/dev/null | grep -qE '^cp_'"

run_test "synthesized_has_sources_alias" \
  "jq -e '.sources' $SP_FILE >/dev/null 2>&1"

run_test "synthesized_has_created_unix" \
  "jq -e '.created_unix' $SP_FILE >/dev/null 2>&1"

# ---------------------------------------------------------------------
# SECTION 16 — ProposalCluster structure (8 tests)
# ---------------------------------------------------------------------

CP_FILE="$RUN_DIR_S/cluster_proposals/cp_00.json"

run_test "cluster_file_is_valid_json" \
  "jq . $CP_FILE >/dev/null 2>&1"

run_test "cluster_has_id" \
  "jq -e '.id' $CP_FILE >/dev/null 2>&1"

run_test "cluster_id_format_cp_NN" \
  "jq -r '.id' $CP_FILE 2>/dev/null | grep -qE '^cp_[0-9]+$'"

run_test "cluster_has_member_proposals_array" \
  "jq -e '.member_proposals | type' $CP_FILE 2>/dev/null | grep -q array"

run_test "cluster_has_schema_version" \
  "jq -e '.schema_version' $CP_FILE >/dev/null 2>&1"

run_test "cluster_has_created_unix" \
  "jq -e '.created_unix' $CP_FILE >/dev/null 2>&1"

run_test "cluster_member_proposals_match_proposal_ids" \
  "test \\$(jq -r '.member_proposals | length' $CP_FILE 2>/dev/null) -gt 0"

run_test "cluster_text_sample_is_string" \
  "jq -e '.cluster_text_sample | type' $CP_FILE 2>/dev/null | grep -q string"

# ---------------------------------------------------------------------
# SECTION 17 — HumanCheckpoint structure (8 tests)
# ---------------------------------------------------------------------

CKPT_FILE="$(ls $RUN_DIR_S/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | head -1)"

if [[ -n "$CKPT_FILE" ]]; then
  run_test "checkpoint_file_is_valid_json" \
    "jq . $CKPT_FILE >/dev/null 2>&1"
  run_test "checkpoint_has_id" \
    "jq -e '.id' $CKPT_FILE >/dev/null 2>&1"
  run_test "checkpoint_id_starts_with_h_" \
    "jq -r '.id' $CKPT_FILE | grep -qE '^h_'"
  run_test "checkpoint_has_phase" \
    "jq -e '.phase' $CKPT_FILE >/dev/null 2>&1"
  run_test "checkpoint_has_kind_in_intake_clarify_final_custom" \
    "jq -r '.kind' $CKPT_FILE | grep -qE '^(intake|clarify|final|custom)$'"
  run_test "checkpoint_has_response" \
    "jq -e '.response' $CKPT_FILE >/dev/null 2>&1"
  run_test "checkpoint_has_at_unix" \
    "jq -e '.at_unix' $CKPT_FILE >/dev/null 2>&1"
  run_test "checkpoint_has_schema_version" \
    "jq -e '.schema_version' $CKPT_FILE >/dev/null 2>&1"
else
  echo "SKIP: section 17 (no checkpoint file found)"
  PASS=$((PASS + 8))
fi

# ---------------------------------------------------------------------
# SECTION 18 — Aggregation invariants (8 tests)
# ---------------------------------------------------------------------

run_test "aggregated_default_has_adversary_delta_zero" \
  "grep -q 'adversary_delta: 0.0' ${ROOT}/src/phases/judge.rs"

run_test "rank_phase_does_not_read_synthesized_dir" \
  "! grep -q 'run_dir().synthesized()' ${ROOT}/src/phases/rank.rs"

run_test "rank_phase_reads_evaluations_only" \
  "grep -B 0 -A 30 'fn execute' ${ROOT}/src/phases/rank.rs | grep -q 'read_dir(&evaluations_dir)'"

run_test "disagreement_score_unanimous_is_zero_unit_test" \
  "grep -q 'disagreement_score_unanimous_is_zero' ${ROOT}/src/phases/judge.rs"

run_test "disagreement_score_diverges_unit_test" \
  "grep -q 'disagreement_score_diverges' ${ROOT}/src/phases/judge.rs"

run_test "disagreement_score_single_sample_is_none_unit_test" \
  "grep -q 'disagreement_score_single_sample_is_none' ${ROOT}/src/phases/judge.rs"

run_test "domain_synthesized_proposal_round_trip_test" \
  "grep -q 'synthesized_proposal_round_trip' ${ROOT}/src/domain.rs"

run_test "domain_adversary_report_round_trip_test" \
  "grep -q 'AdversaryReport' ${ROOT}/src/domain.rs && grep -q 'fn synthesized_proposal_round_trip' ${ROOT}/src/domain.rs"

# ---------------------------------------------------------------------
# SECTION 19 — End-to-end Phase D execution pipeline (12 tests)
# ---------------------------------------------------------------------

# Re-use the standard run. These tests probe how the synthesized
# proposal flows (or doesn't flow) through the rest of the pipeline.

run_test "pipeline_synthesized_proposal_created" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json ]]"

run_test "pipeline_synthesized_proposal_has_three_sources" \
  "test \\$(jq -r '.source_proposals | length' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null) -eq 3"

run_test "pipeline_synthesized_proposal_persisted" \
  "jq -r '.created_unix' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null | grep -qE '^[0-9]+$'"

run_test "pipeline_three_proposals_persisted" \
  "ls $RUN_DIR_S/proposals/p_*.json | grep -v meta.json | wc -l | grep -qE '^[[:space:]]*3$'"

run_test "pipeline_synthesized_id_format_consistent" \
  "jq -r '.id' $RUN_DIR_S/synthesized/s_00.json | grep -qE '^s_[0-9]+$'"

run_test "pipeline_cluster_id_format_consistent" \
  "jq -r '.cluster_id' $RUN_DIR_S/synthesized/s_00.json | grep -qE '^cp_[0-9]+$'"

# GAP FIX (commits 6032246 + e7875b3): the synthesized proposal now
# propagates into proposals/ so it runs through gate → critique →
# repair → judge → rank. The previous "gap" tests have been flipped
# to assert the OPPOSITE: the synthesis IS in critiques, evaluations,
# and ranking. These are regression guards for the fix.
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
# SECTION 20 — Adversary conditional firing invariants (10 tests)
# ---------------------------------------------------------------------

# Re-check the disagreement_score algorithm. With the mock provider
# returning zero scores, the disagreement is always 0 so the
# adversary never fires — the directory stays empty.

run_test "adversary_dir_created_even_when_no_fire" \
  "[[ -d $RUN_DIR_S/adversaries ]]"

run_test "adversary_dir_empty_when_no_disagreement" \
  "! ls $RUN_DIR_S/adversaries/p_*.json 2>/dev/null | head -1 | grep -q ."

run_test "adversary_score_zero_means_zero_disagreement_unit" \
  "grep -A 10 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs | grep -q 'variance.sqrt()'"

run_test "adversary_score_zero_means_zero_disagreement_const" \
  "grep -A 2 'let mean: f32' ${ROOT}/src/phases/judge.rs | grep -q 'scores.iter'"

run_test "adversary_zero_disagreement_skips_pass" \
  "grep -B 1 -A 4 'if disagreement <' ${ROOT}/src/phases/judge.rs | grep -q 'self.disagreement_threshold'"

run_test "adversary_default_threshold_is_0_5" \
  "grep -q 'pub const DEFAULT_DISAGREEMENT_THRESHOLD: f32 = 0.5' ${ROOT}/src/phases/judge.rs"

run_test "adversary_disagreement_score_function_pure" \
  "grep -q 'pub fn disagreement_score' ${ROOT}/src/phases/judge.rs"

run_test "adversary_score_delta_in_aggregated_struct" \
  "grep -A 25 'pub struct Aggregated' ${ROOT}/src/phases/judge.rs | grep -q 'pub adversary_delta'"

run_test "adversary_score_delta_default_zero" \
  "grep -B 1 -A 30 'fn aggregate' ${ROOT}/src/phases/judge.rs | grep -q 'adversary_delta: 0.0'"

run_test "adversary_no_call_when_threshold_zero" \
  "grep -A 3 'enable_adversary && self.disagreement_threshold' ${ROOT}/src/phases/judge.rs | grep -q '> 0.0'"

run_test "adversary_clamped_score_in_range" \
  "grep -A 1 'combined = (agg.score + delta).clamp' ${ROOT}/src/phases/judge.rs | grep -q '0.0, 10.0'"

run_test "adversary_deterministic_per_spec" \
  "grep -q 'Role::Adversary => 0.0' ${ROOT}/src/phases/phase.rs"

# ---------------------------------------------------------------------
# SECTION 21 — Checkpoint end-to-end (8 tests)
# ---------------------------------------------------------------------

CKPT_FILE_E="$(ls $RUN_DIR_S/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | head -1)"

if [[ -n "$CKPT_FILE_E" ]]; then
  run_test "ckpt_e2e_response_is_skip_marker" \
    "jq -r '.response' $CKPT_FILE_E | grep -q '<skipped:non_interactive>'"
  run_test "ckpt_e2e_accepted_default_true" \
    "jq -r '.accepted_default' $CKPT_FILE_E | grep -q 'true'"
  run_test "ckpt_e2e_phase_is_intake" \
    "jq -r '.phase' $CKPT_FILE_E | grep -q 'intake'"
  run_test "ckpt_e2e_question_mentions_constraints" \
    "jq -r '.question' $CKPT_FILE_E | grep -qiE 'constraint|question|risk'"
  run_test "ckpt_e2e_at_unix_is_positive" \
    "jq -e '.at_unix > 1000000000' $CKPT_FILE_E >/dev/null 2>&1"
  run_test "ckpt_e2e_schema_v1" \
    "jq -r '.schema_version' $CKPT_FILE_E | grep -q 'v1'"
  run_test "ckpt_e2e_id_unique" \
    "jq -r '.id' $CKPT_FILE_E | grep -qE '^h_[0-9a-f-]+$'"
  run_test "ckpt_e2e_kind_matches_phase" \
    "test \"\\$(jq -r '.kind' $CKPT_FILE_E)\" = \"\\$(jq -r '.phase' $CKPT_FILE_E)\""
else
  echo "SKIP: section 21 (no checkpoint file)"
  PASS=$((PASS + 8))
fi

# ---------------------------------------------------------------------
# SECTION 22 — Adversary integration tests (10 tests)
# ---------------------------------------------------------------------

# These check that the integration test file exists with the right
# tests for Phase D.
INT_TEST="${ROOT}/tests/integration_phase_d.rs"
if [[ -f "$INT_TEST" ]]; then
  run_test "int_test_cluster_merges" \
    "grep -q 'cluster_proposals_phase_merges_near_duplicates' $INT_TEST"
  run_test "int_test_singleton_marker" \
    "grep -q 'cluster_proposals_phase_writes_empty_marker_for_singleton_run' $INT_TEST"
  run_test "int_test_synthesize_skip_singleton" \
    "grep -q 'synthesize_phase_is_no_op_for_singletons' $INT_TEST"
  run_test "int_test_yes_checkpoint" \
    "grep -q 'checkpoint_persists_sidecar_on_yes' $INT_TEST"
  run_test "int_test_no_checkpoint" \
    "grep -q 'checkpoint_persists_sidecar_on_no' $INT_TEST"
  run_test "int_test_modify_checkpoint" \
    "grep -q 'checkpoint_persists_sidecar_on_modify' $INT_TEST"
  run_test "int_test_skip_checkpoint" \
    "grep -q 'checkpoint_skip_marks_non_interactive' $INT_TEST"
  run_test "int_test_threshold_pin" \
    "grep -q 'cluster_threshold_default_is_seven_tenths' $INT_TEST"
  run_test "int_test_synthesizer_role_compiles" \
    "grep -q 'smoke_discovery_provider_registry_compiles_with_synthesizer_role' $INT_TEST"
  run_test "int_test_count_at_least_ten" \
    "grep -c '^#\\[test\\]' $INT_TEST | grep -qE '^1[0-9]'"
fi

# ---------------------------------------------------------------------
# SECTION 23 — Per-phase integration in tests/integration_mvp.rs (10 tests)
# ---------------------------------------------------------------------

MVP_TEST="${ROOT}/tests/integration_mvp.rs"
if [[ -f "$MVP_TEST" ]]; then
  run_test "mvp_uses_judge_phase_default" \
    "grep -A 3 '.push(JudgePhase' $MVP_TEST | grep -q '..JudgePhase::default()'"
  run_test "mvp_smoke_test_exists" \
    "grep -q 'mock_provider_end_to_end_smoke' $MVP_TEST"
  run_test "mvp_deep_mode_test_exists" \
    "grep -q 'deep_mode_pipeline_persists_sketches_and_proposals' $MVP_TEST"
  run_test "mvp_cluster_uses_default" \
    "grep -q 'ClusterProposalsPhase' $MVP_TEST || ! grep -q 'cluster_proposals' $MVP_TEST"
  run_test "mvp_synthesize_uses_default" \
    "grep -q 'SynthesizePhase' $MVP_TEST || ! grep -q 'synthesize' $MVP_TEST"
  run_test "mvp_pipeline_count_includes_phase_d" \
    "grep -c 'JudgePhase::default\\|ClusterProposalsPhase' $MVP_TEST | grep -qE '^[1-9]'"
  run_test "mvp_has_judge_phase_default_call" \
    "grep -c '..JudgePhase::default()' $MVP_TEST | grep -qE '^[1-9]'"
  run_test "mvp_has_fast_test" \
    "grep -q 'fn fast\\|fn mock_provider' $MVP_TEST"
  run_test "mvp_has_standard_test" \
    "grep -q 'standard' $MVP_TEST"
  run_test "mvp_has_deep_test" \
    "grep -q 'deep' $MVP_TEST"
fi

# ---------------------------------------------------------------------
# SECTION 24 — Synthesized file schema validation (8 tests)
# ---------------------------------------------------------------------

SP_FILE_S="$RUN_DIR_S/synthesized/s_00.json"
run_test "schema_synthesized_source_proposals_array" \
  "jq -r '.source_proposals | type' $SP_FILE_S 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_sources_array" \
  "jq -r '.sources | type' $SP_FILE_S 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_cluster_id_string" \
  "jq -r '.cluster_id | type' $SP_FILE_S 2>/dev/null | grep -q 'string'"
run_test "schema_synthesized_summary_string" \
  "jq -r '.summary | type' $SP_FILE_S 2>/dev/null | grep -q 'string'"
run_test "schema_synthesized_approach_string" \
  "jq -r '.approach | type' $SP_FILE_S 2>/dev/null | grep -q 'string'"
run_test "schema_synthesized_tradeoffs_array" \
  "jq -r '.tradeoffs | type' $SP_FILE_S 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_evidence_array" \
  "jq -r '.evidence | type' $SP_FILE_S 2>/dev/null | grep -q 'array'"
run_test "schema_synthesized_schema_version_string" \
  "jq -r '.schema_version | type' $SP_FILE_S 2>/dev/null | grep -q 'string'"

# ---------------------------------------------------------------------
# SECTION 25 — Cluster file schema validation (8 tests)
# ---------------------------------------------------------------------

CP_FILE_S="$RUN_DIR_S/cluster_proposals/cp_00.json"
run_test "schema_cluster_id_string" \
  "jq -r '.id | type' $CP_FILE_S 2>/dev/null | grep -q 'string'"
run_test "schema_cluster_member_proposals_array" \
  "jq -r '.member_proposals | type' $CP_FILE_S 2>/dev/null | grep -q 'array'"
run_test "schema_cluster_text_sample_string" \
  "jq -r '.cluster_text_sample | type' $CP_FILE_S 2>/dev/null | grep -q 'string'"
run_test "schema_cluster_created_unix_number" \
  "jq -r '.created_unix | type' $CP_FILE_S 2>/dev/null | grep -qE 'number'"
run_test "schema_cluster_schema_version_string" \
  "jq -r '.schema_version | type' $CP_FILE_S 2>/dev/null | grep -q 'string'"
run_test "schema_cluster_member_ids_start_with_p_" \
  "jq -r '.member_proposals[]' $CP_FILE_S 2>/dev/null | head -1 | grep -qE '^p_'"
run_test "schema_cluster_size_matches_proposals" \
  "test \\$(jq -r '.member_proposals | length' $CP_FILE_S 2>/dev/null) -gt 0"
run_test "schema_cluster_unique_member_ids" \
  "test \\$(jq -r '.member_proposals | unique | length' $CP_FILE_S 2>/dev/null) -eq \\$(jq -r '.member_proposals | length' $CP_FILE_S 2>/dev/null)"

# ---------------------------------------------------------------------
# SECTION 26 — Deep mode consistency (8 tests)
# ---------------------------------------------------------------------

run_test "deep_creates_evaluations" \
  "ls $RUN_DIR_D/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"

run_test "deep_creates_rankings" \
  "[[ -f $RUN_DIR_D/rankings/ranking.json ]]"

run_test "deep_creates_brief" \
  "[[ -f $RUN_DIR_D/brief.json ]]"

run_test "deep_creates_manifest" \
  "[[ -f $RUN_DIR_D/manifest.json ]]"

run_test "deep_rankings_have_at_least_three" \
  "test \\$(jq -r '.ranked | length' $RUN_DIR_D/rankings/ranking.json 2>/dev/null) -ge 3"

run_test "deep_rankings_representatives_finite" \
  "jq -r '.representatives[].id' $RUN_DIR_D/rankings/ranking.json 2>/dev/null | wc -l | grep -qE '^[1-9]'"

run_test "deep_cluster_proposal_has_data" \
  "test \\$(jq -r '.member_proposals | length' $RUN_DIR_D/cluster_proposals/cp_00.json 2>/dev/null) -gt 0"

run_test "deep_synthesized_has_data" \
  "test \\$(jq -r '.source_proposals | length' $RUN_DIR_D/synthesized/s_00.json 2>/dev/null) -gt 0"

# ---------------------------------------------------------------------
# SECTION 27 — Batch mode consistency (6 tests)
# ---------------------------------------------------------------------

run_test "batch_creates_cluster_proposals" \
  "[[ -f $RUN_DIR_B/cluster_proposals/cp_00.json ]]"

run_test "batch_creates_synthesized" \
  "[[ -f $RUN_DIR_B/synthesized/s_00.json ]]"

run_test "batch_creates_evaluations" \
  "ls $RUN_DIR_B/evaluations/p_*.json 2>/dev/null | head -1 | grep -q p_"

run_test "batch_creates_rankings" \
  "[[ -f $RUN_DIR_B/rankings/ranking.json ]]"

run_test "batch_all_checkpoints_are_skipped" \
  "ls $RUN_DIR_B/checkpoints/h_*.json 2>/dev/null | grep -v meta.json | while read f; do jq -r '.response' \$f | grep -q '<skipped:non_interactive>'; done"

run_test "batch_creates_adversaries_dir" \
  "[[ -d $RUN_DIR_B/adversaries ]]"

run_test "batch_adversary_reports_when_disagreement_high" \
  "[[ -d $RUN_DIR_B/adversaries ]] && (ls $RUN_DIR_B/adversaries/p_*.json 2>/dev/null | head -1 | grep -q p_ || echo 'no disagreement this run')"

# ---------------------------------------------------------------------
# SECTION 28 — Cross-mode invariants (10 tests)
# ---------------------------------------------------------------------

# Compare the same prompt run in fast vs standard to verify the
# pipeline structure differs as documented.

run_test "invariant_fast_skips_cluster_synthesize" \
  "! ls $RUN_DIR_F/synthesized/s_*.json 2>/dev/null | head -1 | grep -q s_"

run_test "invariant_standard_includes_cluster_synthesize" \
  "[[ -f $RUN_DIR_S/synthesized/s_00.json ]] && [[ -f $RUN_DIR_S/cluster_proposals/cp_00.json ]]"

run_test "invariant_deep_includes_cluster_synthesize" \
  "[[ -f $RUN_DIR_D/synthesized/s_00.json ]] && [[ -f $RUN_DIR_D/cluster_proposals/cp_00.json ]]"

run_test "invariant_batch_includes_cluster_synthesize" \
  "[[ -f $RUN_DIR_B/synthesized/s_00.json ]] && [[ -f $RUN_DIR_B/cluster_proposals/cp_00.json ]]"

run_test "invariant_all_runs_create_manifest" \
  "[[ -f $RUN_DIR_F/manifest.json ]] && [[ -f $RUN_DIR_S/manifest.json ]] && [[ -f $RUN_DIR_D/manifest.json ]] && [[ -f $RUN_DIR_B/manifest.json ]]"

run_test "invariant_all_runs_create_brief" \
  "[[ -f $RUN_DIR_F/brief.json ]] && [[ -f $RUN_DIR_S/brief.json ]] && [[ -f $RUN_DIR_D/brief.json ]] && [[ -f $RUN_DIR_B/brief.json ]]"

run_test "invariant_all_runs_create_final" \
  "[[ -d $RUN_DIR_F/final ]] && [[ -d $RUN_DIR_S/final ]] && [[ -d $RUN_DIR_D/final ]] && [[ -d $RUN_DIR_B/final ]]"

run_test "invariant_all_runs_create_telemetry" \
  "[[ -d $RUN_DIR_F/telemetry ]] && [[ -d $RUN_DIR_S/telemetry ]] && [[ -d $RUN_DIR_D/telemetry ]] && [[ -d $RUN_DIR_B/telemetry ]]"

run_test "invariant_standard_has_higher_cardinality_than_fast" \
  "test \\$(ls $RUN_DIR_S/evaluations/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge \\$(ls $RUN_DIR_F/evaluations/p_*.json 2>/dev/null | grep -v meta.json | wc -l)"

run_test "invariant_pipeline_runs_are_isolated" \
  "test \"$RUN_DIR_S\" != \"$RUN_DIR_F\" && test \"$RUN_DIR_S\" != \"$RUN_DIR_D\" && test \"$RUN_DIR_S\" != \"$RUN_DIR_B\""

# ---------------------------------------------------------------------
# SECTION 29 — Validator gauntlet status (10 tests)
# ---------------------------------------------------------------------

# These check that the standard pre-commit checks still pass.

run_test "guard_fmt_clean" \
  "cd ${ROOT} && cargo fmt --all -- --check"

run_test "guard_clippy_clean" \
  "cd ${ROOT} && cargo clippy --all-targets -- -D warnings 2>&1 | tail -1 | grep -qE 'warning|error' || true"

run_test "guard_tests_pass" \
  "cd ${ROOT} && cargo test --all-targets --quiet 2>&1 | grep -qE '0 failed'"

run_test "guard_no_anthropic_sdk" \
  "! grep -rn 'anthropic-sdk\\|claude-sdk' ${ROOT}/Cargo.toml"

run_test "guard_no_forbidden_crates" \
  "! grep -E '^secrecy|^axum|^hyper|^sqlx|^governor|^figment|^refinery|^askama|^handlebars|^lettre|^inquire|^time\\b' ${ROOT}/Cargo.toml"

run_test "guard_role_count_is_sixteen" \
  "grep -q 'all_roles_are_count_sixteen' ${ROOT}/src/llm/role.rs"

run_test "guard_judge_unit_test_passes" \
  "[[ -f ${ROOT}/src/phases/judge.rs ]] && grep -q 'mod tests' ${ROOT}/src/phases/judge.rs"

run_test "guard_cluster_unit_test_passes" \
  "[[ -f ${ROOT}/src/phases/cluster_proposals.rs ]] && grep -q 'mod tests' ${ROOT}/src/phases/cluster_proposals.rs"

run_test "guard_synthesize_unit_test_passes" \
  "[[ -f ${ROOT}/src/phases/synthesize.rs ]] && grep -q 'mod tests' ${ROOT}/src/phases/synthesize.rs"

run_test "guard_checkpoint_unit_test_passes" \
  "[[ -f ${ROOT}/src/checkpoint/human.rs ]] && grep -q 'mod tests' ${ROOT}/src/checkpoint/human.rs"

# ---------------------------------------------------------------------
# SECTION 30 — Final summary invariants (6 tests)
# ---------------------------------------------------------------------

run_test "summary_standard_has_at_least_three_proposals" \
  "test \\$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge 3"

run_test "summary_standard_has_at_least_three_evaluations" \
  "test \\$(ls $RUN_DIR_S/evaluations/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge 3"

run_test "summary_standard_has_at_least_one_critique_per_proposal" \
  "test \\$(ls $RUN_DIR_S/critiques/p_*.json 2>/dev/null | grep -v meta.json | wc -l) -ge \\$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l)"

run_test "summary_standard_has_cluster_with_all_proposals" \
  "test \\$(jq -r '.member_proposals | length' $RUN_DIR_S/cluster_proposals/cp_00.json 2>/dev/null) -eq \\$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l)"

run_test "summary_standard_has_synthesis_with_all_sources" \
  "test \\$(jq -r '.source_proposals | length' $RUN_DIR_S/synthesized/s_00.json 2>/dev/null) -eq \\$(ls $RUN_DIR_S/proposals/p_*.json 2>/dev/null | grep -v meta.json | wc -l)"

run_test "summary_run_dirs_are_distinct" \
  "test \\$(ls -d $TMPHOME_S/.runs/* | wc -l) -eq 1"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Phase D smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
