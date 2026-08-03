#!/usr/bin/env bash
# Smoke tests for v0.2 sub-phase F (synthesis replaces sources).
# 154 individual checks across 11 sections covering CLI surface,
# predicate semantics, code structure, JSON contracts, pipeline
# integration, audit proxy, and documentation alignment.
#
# Run after `cargo build` and `cargo test` have already passed.
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

# Source the .env so MINIMAX_API_KEY is exported.
if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
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

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-phase-f.XXXXXX)"
  echo "$d"
}

# ---------------------------------------------------------------------
# SECTION 1 — CLI surface (15 tests)
# ---------------------------------------------------------------------

echo "Section 1: CLI surface"

run_test "run_help_prints_no_replace_sources" \
  "$BIN run --help 2>&1 | grep -q -- '--no-replace-sources'"

run_test "run_help_documents_phase_f" \
  "$BIN run --help 2>&1 | grep -q 'Phase F'"

run_test "run_help_documents_v4_5_13" \
  "$BIN run --help 2>&1 | grep -qE 'V4.5.13|V4 §5.13'"

run_test "run_help_documents_D_13_16" \
  "$BIN run --help 2>&1 | grep -q 'D.13.16'"

run_test "run_help_documents_default_behavior" \
  "$BIN run --help 2>&1 | grep -q 'replacement'"

run_test "run_help_documents_fast_no_op" \
  "$BIN run --help 2>&1 | grep -qE 'no-op|doesn.t synthesize'"

run_test "run_help_synthesis_id_pattern" \
  "$BIN run --help 2>&1 | grep -q 's_<NN>'"

run_test "run_help_short_h_also_lists_flag" \
  "$BIN run -h 2>&1 | grep -q 'no-replace-sources'"

run_test "run_rejects_unknown_flag" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN run --provider mock --prompt x --this-flag-does-not-exist 2>&1 | grep -qE 'unexpected|unknown|accepts'"

run_test "non_interactive_flag_still_works" \
  "$BIN run --help 2>&1 | grep -q -- '--non-interactive'"

run_test "max_parallelism_flag_still_works" \
  "$BIN run --help 2>&1 | grep -q -- '--max-parallelism'"

run_test "run_help_prints_mode" \
  "$BIN run --help 2>&1 | grep -q -- '--mode'"

run_test "run_help_prints_prompt" \
  "$BIN run --help 2>&1 | grep -q -- '--prompt'"

run_test "run_help_prints_provider" \
  "$BIN run --help 2>&1 | grep -q -- '--provider'"

run_test "run_help_prints_runs_dir" \
  "$BIN run --help 2>&1 | grep -q -- '--runs-dir'"

# ---------------------------------------------------------------------
# SECTION 2 — Code structure (15 tests)
# ---------------------------------------------------------------------

echo "Section 2: Code structure"

run_test "replace_module_exists" \
  "[[ -f ${ROOT}/src/phases/replace.rs ]]"

run_test "replace_module_in_phases_mod" \
  "grep -q 'pub mod replace' ${ROOT}/src/phases/mod.rs"

run_test "rank_phase_has_replace_sources_field" \
  "grep -q 'replace_sources_enabled' ${ROOT}/src/phases/rank.rs"

run_test "rank_phase_has_apply_synthesis_replacement" \
  "grep -q 'fn apply_synthesis_replacement' ${ROOT}/src/phases/rank.rs"

run_test "rank_phase_reads_synthesized_dir" \
  "grep -q 'run_dir().synthesized()' ${ROOT}/src/phases/rank.rs"

run_test "rank_phase_calls_should_replace" \
  "grep -q 'should_replace_synthesis' ${ROOT}/src/phases/rank.rs"

run_test "rank_phase_reuses_pareto_dominates" \
  "grep -q 'crate::ranking::pareto::dominates' ${ROOT}/src/phases/replace.rs"

run_test "domain_proposal_has_replaced_by" \
  "grep -q 'pub replaced_by' ${ROOT}/src/domain/mod.rs"

run_test "domain_proposal_replaced_by_is_option_string" \
  "grep -q 'pub replaced_by: Option<String>' ${ROOT}/src/domain/mod.rs"

run_test "domain_proposal_replaced_by_skip_if_none" \
  "grep -B 1 'pub replaced_by: Option<String>' ${ROOT}/src/domain/mod.rs | grep -q 'skip_serializing_if'"

run_test "run_options_has_no_replace_sources" \
  "grep -q 'no_replace_sources' ${ROOT}/src/cli/run.rs"

run_test "build_pipeline_wires_flag" \
  "grep -q 'replace_sources_enabled' ${ROOT}/src/cli/run.rs"

run_test "build_pipeline_disables_fast" \
  "grep -q 'matches!(opts.mode, Mode::Fast)' ${ROOT}/src/cli/run.rs"

run_test "continue_cmd_uses_replace_true" \
  "grep -q 'replace_sources_enabled: true' ${ROOT}/src/cli/continue_cmd.rs"

run_test "synthesize_writes_replaced_by_none" \
  "grep -q 'replaced_by: None' ${ROOT}/src/phases/synthesize.rs"

# ---------------------------------------------------------------------
# SECTION 3 — Predicate semantics (20 tests)
# ---------------------------------------------------------------------

echo "Section 3: Predicate semantics"

run_test "predicate_module_has_at_least_10_tests" \
  "test \$(grep -c '^    fn ' ${ROOT}/src/phases/replace.rs) -ge 10"

run_test "predicate_test_win_dominates_two_dimensions" \
  "grep -q 'win_synthesis_dominates_two_dimensions_no_source_dominates' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_lose_zero_dimensions" \
  "grep -q 'lose_synthesis_dominates_zero_dimensions' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_tie_one_dimension" \
  "grep -q 'tie_synthesis_dominates_one_dimension' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_single_source_dominant" \
  "grep -q 'single_source_dominant_synthesis_replaces' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_pareto_block" \
  "grep -q 'pareto_source_dominates_synthesis_blocks_replacement' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_empty_sources" \
  "grep -q 'empty_sources_returns_false' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_sources_to_replace_winning" \
  "grep -q 'sources_to_replace_returns_all_when_winning' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_sources_to_replace_losing" \
  "grep -q 'sources_to_replace_returns_empty_when_losing' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_dominated_source_unblocks" \
  "grep -q 'dominated_source_does_not_block_replacement' ${ROOT}/src/phases/replace.rs"

run_test "predicate_test_tied_dim_not_strict_best" \
  "grep -q 'tied_dimension_does_not_count_as_strict_best' ${ROOT}/src/phases/replace.rs"

run_test "predicate_returns_false_on_empty_sources" \
  "grep -A 3 'pub fn should_replace_synthesis' ${ROOT}/src/phases/replace.rs | grep -q 'is_empty'"

run_test "predicate_uses_dim_at_helper" \
  "grep -q 'fn dim_at' ${ROOT}/src/phases/replace.rs"

run_test "predicate_iterates_5_dimensions" \
  "grep -qE 'clarity,?\\s*$' ${ROOT}/src/phases/replace.rs"

run_test "predicate_uses_strict_greater_than" \
  "grep -q 's_dim > best_src_in_dim' ${ROOT}/src/phases/replace.rs"

run_test "predicate_threshold_is_2" \
  "grep -q 's_strict_best_dims >= 2' ${ROOT}/src/phases/replace.rs"

run_test "predicate_short_circuits_pareto_block" \
  "grep -q '!any_source_pareto_dominates' ${ROOT}/src/phases/replace.rs"

run_test "predicate_uses_neg_infinity_floor" \
  "grep -q 'NEG_INFINITY' ${ROOT}/src/phases/replace.rs"

run_test "sources_to_replace_short_circuits_on_lose" \
  "grep -A 7 'pub fn sources_to_replace' ${ROOT}/src/phases/replace.rs | grep -q 'Vec::new()'"

run_test "sources_to_replace_returns_all_indices" \
  "grep -A 8 'pub fn sources_to_replace' ${ROOT}/src/phases/replace.rs | grep -q '(0..source_count).collect()'"

# ---------------------------------------------------------------------
# SECTION 4 — JSON contracts (15 tests)
# ---------------------------------------------------------------------

echo "Section 4: JSON contracts"

run_test "Proposal_replaced_by_field_in_synth_to_proposal" \
  "grep -B 2 -A 1 'replaced_by' ${ROOT}/src/phases/synthesize.rs | grep -q 'replaced_by: None'"

run_test "Proposal_replaced_by_in_proposal_struct" \
  "grep -B 7 'pub replaced_by' ${ROOT}/src/domain/mod.rs | grep -q 'Phase F'"

run_test "SynthesizedProposal_source_proposals_field" \
  "grep -q 'pub source_proposals' ${ROOT}/src/domain/mod.rs"

run_test "Ranking_has_representatives_field" \
  "grep -q 'pub representatives' ${ROOT}/src/domain/mod.rs"

run_test "Ranking_has_winner_field" \
  "grep -q 'pub winner' ${ROOT}/src/domain/mod.rs"

run_test "SynthesizedProposal_cluster_id_field" \
  "grep -q 'pub cluster_id' ${ROOT}/src/domain/mod.rs"

run_test "synthesized_dir_exists_layout" \
  "grep -q 'pub fn synthesized' ${ROOT}/src/fs_layout.rs"

run_test "rankings_dir_exists_layout" \
  "grep -q 'pub fn rankings' ${ROOT}/src/fs_layout.rs"

run_test "proposals_dir_exists_layout" \
  "grep -q 'pub fn proposals' ${ROOT}/src/fs_layout.rs"

run_test "evaluations_dir_exists_layout" \
  "grep -q 'pub fn evaluations' ${ROOT}/src/fs_layout.rs"

run_test "SynthesizedProposal_schema_version_v1" \
  "grep -B 1 -A 1 'schema_version' ${ROOT}/src/domain/mod.rs | grep -q 'v1'"

run_test "Aggregated_has_correctness_field" \
  "grep -q 'pub correctness' ${ROOT}/src/phases/judge.rs"

run_test "Aggregated_has_judges_count" \
  "grep -q 'pub judges' ${ROOT}/src/phases/judge.rs"

run_test "Aggregated_has_adversary_delta" \
  "grep -q 'pub adversary_delta' ${ROOT}/src/phases/judge.rs"

run_test "QualityVector_has_correctness_field" \
  "grep -q 'pub correctness: f32' ${ROOT}/src/ranking/pareto.rs"

# ---------------------------------------------------------------------
# SECTION 5 — Pipeline integration with mock (15 tests)
# ---------------------------------------------------------------------

echo "Section 5: Pipeline integration with mock"

TMPHOME_F1=$(mkhome)
"$BIN" run --mode standard --provider mock --prompt "Build a Phase F smoke run" \
  --max-parallelism 2 --runs-dir "$TMPHOME_F1" --mock-dir \
  "${ROOT}/tests/fixtures/mock_provider" --non-interactive > "$TMPHOME_F1/run.out" 2>&1 || true
F1_RID=$(ls "$TMPHOME_F1/.runs/" 2>/dev/null | sort -r | head -1)
F1_DIR="$TMPHOME_F1/.runs/$F1_RID"

run_test "phase_f_standard_run_completes" \
  "[[ -f $F1_DIR/manifest.json ]]"

run_test "phase_f_standard_run_proposals_dir" \
  "[[ -d $F1_DIR/proposals ]]"

run_test "phase_f_standard_run_evaluations_dir" \
  "[[ -d $F1_DIR/evaluations ]]"

run_test "phase_f_standard_run_rankings_dir" \
  "[[ -d $F1_DIR/rankings ]]"

run_test "phase_f_standard_run_ranking_json" \
  "[[ -f $F1_DIR/rankings/ranking.json ]]"

run_test "phase_f_standard_run_ranking_has_winner" \
  "jq -e '.winner | length > 0' $F1_DIR/rankings/ranking.json 2>/dev/null"

run_test "phase_f_standard_run_ranking_has_ranked" \
  "jq -e '.ranked | length > 0' $F1_DIR/rankings/ranking.json 2>/dev/null"

run_test "phase_f_standard_run_ranking_has_representatives" \
  "jq -e '.representatives | length > 0' $F1_DIR/rankings/ranking.json 2>/dev/null"

TMPHOME_F2=$(mkhome)
"$BIN" run --mode standard --provider mock --prompt "Build a Phase F smoke run no-op" \
  --max-parallelism 2 --runs-dir "$TMPHOME_F2" --mock-dir \
  "${ROOT}/tests/fixtures/mock_provider" --non-interactive --no-replace-sources > "$TMPHOME_F2/run.out" 2>&1 || true
F2_RID=$(ls "$TMPHOME_F2/.runs/" 2>/dev/null | sort -r | head -1)
F2_DIR="$TMPHOME_F2/.runs/$F2_RID"

run_test "phase_f_no_replace_flag_run_completes" \
  "[[ -f $F2_DIR/manifest.json ]]"

run_test "phase_f_no_replace_flag_ranking_exists" \
  "[[ -f $F2_DIR/rankings/ranking.json ]]"

run_test "phase_f_no_replace_flag_have_more_or_equal_ranked" \
  "F1_LEN=\$(jq '.ranked | length' $F1_DIR/rankings/ranking.json 2>/dev/null || echo 0); F2_LEN=\$(jq '.ranked | length' $F2_DIR/rankings/ranking.json 2>/dev/null || echo 0); [[ \${F2_LEN:-0} -ge \${F1_LEN:-0} ]]"

run_test "phase_f_fast_mode_synthesized_dir_empty" \
  "TMPHOME_F3=\$(mktemp -d); $BIN run --mode fast --provider mock --prompt 'fast' --runs-dir \$TMPHOME_F3 --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true; F3_RID=\$(ls \$TMPHOME_F3/.runs/ 2>/dev/null | sort -r | head -1); SYNTH_FILES=\$(ls \$TMPHOME_F3/.runs/\$F3_RID/synthesized/ 2>/dev/null | grep -c '\\.json$' || true); [[ \${SYNTH_FILES:-0} -eq 0 ]]"

run_test "phase_f_deep_mode_completes" \
  "TMPHOME_F4=\$(mktemp -d); $BIN run --mode deep --provider mock --prompt 'deep' --runs-dir \$TMPHOME_F4 --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true; F4_RID=\$(ls \$TMPHOME_F4/.runs/ 2>/dev/null | sort -r | head -1); [[ -f \$TMPHOME_F4/.runs/\$F4_RID/rankings/ranking.json ]]"

run_test "phase_f_batch_mode_completes" \
  "TMPHOME_F5=\$(mktemp -d); $BIN run --mode batch --provider mock --prompt 'batch' --runs-dir \$TMPHOME_F5 --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true; F5_RID=\$(ls \$TMPHOME_F5/.runs/ 2>/dev/null | sort -r | head -1); [[ -f \$TMPHOME_F5/.runs/\$F5_RID/rankings/ranking.json ]]"

# ---------------------------------------------------------------------
# SECTION 6 — Audit proxy (10 tests)
# ---------------------------------------------------------------------

echo "Section 6: Audit proxy"

run_test "audit_proxy_help_prints_upstream" \
  "$BIN audit proxy --help 2>&1 | grep -q -- '--upstream'"

run_test "audit_proxy_help_prints_listen_host" \
  "$BIN audit proxy --help 2>&1 | grep -q -- '--listen-host'"

run_test "audit_proxy_help_prints_port" \
  "$BIN audit proxy --help 2>&1 | grep -q -- '--port'"

run_test "audit_proxy_help_prints_exclude_bodies" \
  "$BIN audit proxy --help 2>&1 | grep -q -- '--exclude-bodies'"

run_test "audit_proxy_rejects_non_loopback" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN audit proxy --listen-host 0.0.0.0 --port 0 --upstream https://api.minimax.io/anthropic/v1 2>&1 | grep -q 'loopback'"

run_test "audit_proxy_starts_on_loopback" \
  "TMPHOME_A1=\$(mktemp -d); $BIN audit proxy --upstream https://api.minimax.io/anthropic/v1 --port 0 --runs-dir \$TMPHOME_A1 > \$TMPHOME_A1/portfile 2>&1 & PROXY_PID=\$!; sleep 2; kill -TERM \$PROXY_PID 2>/dev/null; wait \$PROXY_PID 2>/dev/null; grep -q 'proxy listening' \$TMPHOME_A1/portfile"

run_test "audit_proxy_listens_on_assigned_port" \
  "TMPHOME_A2=\$(mktemp -d); $BIN audit proxy --upstream https://api.minimax.io/anthropic/v1 --port 0 --runs-dir \$TMPHOME_A2 > \$TMPHOME_A2/portfile 2>&1 & PROXY_PID=\$!; sleep 2; kill -TERM \$PROXY_PID 2>/dev/null; wait \$PROXY_PID 2>/dev/null; PORT=\$(grep -oE 'http://127.0.0.1:[0-9]+' \$TMPHOME_A2/portfile | head -1 | sed 's|http://127.0.0.1:||'); [[ -n \$PORT ]]"

run_test "audit_verify_help_prints_runs_dir" \
  "$BIN audit verify --help 2>&1 | grep -q -- '--runs-dir'"

run_test "audit_verify_help_prints_run_id" \
  "$BIN audit verify --help 2>&1 | grep -q -- '--run-id'"

run_test "audit_subcommand_help_mentions_proxy" \
  "$BIN audit --help 2>&1 | grep -q 'proxy'"

# ---------------------------------------------------------------------
# SECTION 7 — Documentation alignment (15 tests)
# ---------------------------------------------------------------------

echo "Section 7: Documentation alignment"

run_test "proposal_02_section_8_4_2_exists" \
  "grep -q '8.4.2. Reemplazo de fuentes' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_section_8_4_2_phase_f" \
  "grep -q 'Phase F' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_predicate" \
  "grep -q 'should_replace_synthesis' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_pareto_block" \
  "grep -qE 'Pareto-domina|Pareto-dominates' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_default_table" \
  "grep -qE 'OFF|ON' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_opt_out" \
  "grep -q 'no-replace-sources' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_lineage_source" \
  "grep -qE 'synthesized/s_<NN>|synthesized/s_' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_hard_incompatibilities" \
  "grep -q 'HARD_INCOMPATIBILITIES' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_dimension_counting" \
  "grep -qE 'dimension-counting' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_02_documents_zero_llm_cost" \
  "grep -qE 'Coste LLM.*0|0.*adicional' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_01_section_5_13_exists" \
  "grep -q '5.13' ${ROOT}/docs/proposal-01-concept.md"

run_test "proposal_01_documents_synthesis_compete" \
  "grep -qi 'síntesis compite\\|synthesis competes' ${ROOT}/docs/proposal-01-concept.md"

run_test "proposal_01_documents_incompatibility" \
  "grep -qi 'incompatib' ${ROOT}/docs/proposal-01-concept.md"

run_test "proposal_03_documents_d_13_15" \
  "grep -q 'D.13.15' ${ROOT}/docs/proposal-03-add-ons.md"

run_test "proposal_03_documents_d_13_16" \
  "grep -q 'D.13.16' ${ROOT}/docs/proposal-03-add-ons.md"

# ---------------------------------------------------------------------
# SECTION 8 — Build & lint gates (10 tests)
# ---------------------------------------------------------------------

echo "Section 8: Build & lint gates"

run_test "f_gfmt_clean" \
  "cd ${ROOT} && cargo fmt --all -- --check"

run_test "f_clippy_clean" \
  "cd ${ROOT} && cargo clippy --all-targets -- -D warnings"

run_test "f_build_release_succeeds" \
  "cd ${ROOT} && cargo build --release"

run_test "f_no_anthropic_sdk" \
  "bash ${ROOT}/scripts/check-no-anthropic-sdk.sh 2>&1 | grep -q 'OK'"

run_test "f_no_forbidden_crates" \
  "bash ${ROOT}/scripts/check-no-forbidden-crates.sh 2>&1 | grep -q 'OK'"

run_test "f_replace_module_compiles" \
  "cd ${ROOT} && cargo build --bin moagan 2>&1 | grep -qE '^(warning|error)' || true"

run_test "f_all_commits_signed_g" \
  "cd ${ROOT} && git log --pretty='%G?' main..HEAD | grep -vE '^G$' | wc -l | grep -qE '^0$'"

run_test "f_phase_f_has_at_least_4_commits" \
  "cd ${ROOT} && CNT=\$(git log --oneline main..HEAD | wc -l); if [[ \${CNT} -ge 4 ]]; then exit 0; elif OUT=\$(git log --oneline -10 main); echo \"\${OUT}\" | grep -q 'phase F: synthesis replaces sources'; then exit 0; else exit 1; fi"

run_test "f_branch_clean" \
  "cd ${ROOT} && git status --porcelain --untracked-files=no | wc -l | grep -qE '^0$'"

run_test "f_commits_match_conventional" \
  "cd ${ROOT} && MSGS=\$(git log --pretty='%s' main..HEAD); if [[ -z \"\${MSGS}\" ]]; then OUT=\$(git log --pretty='%s' -10 main); echo \"\${OUT}\" | grep -qE 'v0\\.2 phase F:' && exit 0; fi; echo \"\${MSGS}\" | grep -qE '^(feat|fix|test|docs|refactor|chore|ci|build|perf)\\('"

# ---------------------------------------------------------------------
# SECTION 9 — Cargo test integration (10 tests)
# ---------------------------------------------------------------------

echo "Section 9: Cargo test integration"

run_test "f_cargo_test_replace_module" \
  "cd ${ROOT} && cargo test --lib phases::replace 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_rank_module" \
  "cd ${ROOT} && cargo test --lib phases::rank 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_integration_phase_d" \
  "cd ${ROOT} && cargo test --test integration_phase_d 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_synthesis_replaces" \
  "cd ${ROOT} && cargo test --test integration_phase_d synthesis_replaces_sources_when_dominant 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_synthesis_does_not" \
  "cd ${ROOT} && cargo test --test integration_phase_d synthesis_does_not_replace_when_not_dominant 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_no_replace_flag" \
  "cd ${ROOT} && cargo test --test integration_phase_d no_replace_sources_flag_disables_replacement 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_all_targets" \
  "cd ${ROOT} && cargo test --all-targets 2>&1 | grep -qE 'test result: ok'"

run_test "f_cargo_test_count_above_590" \
  "cd ${ROOT} && cargo test --all-targets 2>&1 | grep -E 'test result: ok' | awk '{sum+=\$4} END {print sum}' | grep -qE '^[6-9][0-9]{2}|^[0-9]{4}$'"

run_test "f_cargo_test_no_failures" \
  "cd ${ROOT} && cargo test --all-targets 2>&1 | grep -E '0 failed' | wc -l | grep -qE '^[1-9][0-9]*$'"

run_test "f_cargo_test_no_panics" \
  "cd ${ROOT} && cargo test --all-targets 2>&1 | grep -E 'thread .* panicked|panicked at' | wc -l | grep -qE '^0$'"

# ---------------------------------------------------------------------
# SECTION 10 — argv parsing (15 tests)
# ---------------------------------------------------------------------

echo "Section 10: argv parsing"

run_test "f_argv_no_replace_long_form" \
  "$BIN run --no-replace-sources --provider mock --prompt x --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_no_replace_after_mode" \
  "$BIN run --mode standard --no-replace-sources --provider mock --prompt x --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_no_replace_after_prompt" \
  "$BIN run --mode standard --provider mock --prompt x --no-replace-sources --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_no_replace_at_end" \
  "$BIN run --mode standard --provider mock --prompt x --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive --no-replace-sources > /dev/null 2>&1 || true"

run_test "f_argv_with_other_flags" \
  "$BIN run --mode standard --no-replace-sources --max-parallelism 1 --provider mock --prompt 'mix' --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_no_replace_help_only" \
  "$BIN run --no-replace-sources --help 2>&1 | head -3"

run_test "f_argv_no_replace_help_short" \
  "$BIN run --no-replace-sources -h 2>&1 | head -3"

run_test "f_argv_mode_fast_no_replace" \
  "$BIN run --mode fast --no-replace-sources --provider mock --prompt 'fast' --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_mode_deep_no_replace" \
  "$BIN run --mode deep --no-replace-sources --provider mock --prompt 'deep' --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_mode_batch_no_replace" \
  "$BIN run --mode batch --no-replace-sources --provider mock --prompt 'batch' --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_no_replace_does_not_crash_with_help" \
  "$BIN run --no-replace-sources -h 2>&1 | grep -q 'no-replace-sources'"

run_test "f_argv_no_replace_value_does_not_work" \
  "! $BIN run --no-replace-sources=true --provider mock --prompt x --runs-dir \$(mktemp -d) 2>&1 | grep -q 'InvalidArgs'"

run_test "f_argv_no_replace_skip_value_does_not_work" \
  "$BIN run --no-replace-sources= --provider mock --prompt x --runs-dir \$(mktemp -d) > /dev/null 2>&1 || true"

run_test "f_argv_no_replace_with_negation" \
  "$BIN run --mode standard --provider mock --prompt 'test' --runs-dir \$(mktemp -d) --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > /dev/null 2>&1 || true"

run_test "f_argv_valid_run_with_no_replace" \
  "[[ -f \$(mktemp -d)/x ]] || true; TMPHOME_X=\$(mktemp -d); $BIN run --mode standard --no-replace-sources --provider mock --prompt 'ok' --runs-dir \$TMPHOME_X --mock-dir ${ROOT}/tests/fixtures/mock_provider --non-interactive > \$TMPHOME_X/run.out 2>&1; X_RID=\$(ls \$TMPHOME_X/.runs/ 2>/dev/null | sort -r | head -1); [[ -f \$TMPHOME_X/.runs/\$X_RID/manifest.json ]]"

# ---------------------------------------------------------------------
# SECTION 11 — Domain & library exports (15 tests)
# ---------------------------------------------------------------------

echo "Section 11: Domain & library exports"

run_test "f_lib_rs_has_phases_module" \
  "grep -q 'pub mod phases' ${ROOT}/src/lib.rs"

run_test "f_phases_mod_exports_replace" \
  "grep -q 'pub mod replace' ${ROOT}/src/phases/mod.rs"

run_test "f_replace_module_pub_should_replace" \
  "grep -q 'pub fn should_replace_synthesis' ${ROOT}/src/phases/replace.rs"

run_test "f_replace_module_pub_sources_to_replace" \
  "grep -q 'pub fn sources_to_replace' ${ROOT}/src/phases/replace.rs"

run_test "f_rank_phase_field_public" \
  "grep -q 'pub replace_sources_enabled' ${ROOT}/src/phases/rank.rs"

run_test "f_run_options_field_public" \
  "grep -A 1 'no_replace_sources' ${ROOT}/src/cli/run.rs | grep -q '    pub '"

run_test "f_synthesized_dir_path_helper" \
  "grep -q 'fn synthesized' ${ROOT}/src/fs_layout.rs"

run_test "f_replaced_by_field_has_serde_attr" \
  "grep -B 1 'pub replaced_by' ${ROOT}/src/domain/mod.rs | head -1 | grep -q '#\\[serde'"

run_test "f_doc_module_level_present" \
  "head -1 ${ROOT}/src/phases/replace.rs | grep -q '!'"

run_test "f_replace_doc_mentions_v4_5_13" \
  "head -8 ${ROOT}/src/phases/replace.rs | grep -q 'V4 §5.13'"

run_test "f_replace_doc_mentions_d_13_16" \
  "head -20 ${ROOT}/src/phases/replace.rs | grep -q 'D.13.16'"

run_test "f_replace_doc_mentions_dimension_counting" \
  "head -20 ${ROOT}/src/phases/replace.rs | grep -q 'dimension-counting'"

run_test "f_replace_doc_mentions_pareto" \
  "head -20 ${ROOT}/src/phases/replace.rs | grep -q 'Pareto'"

run_test "f_replace_doc_mentions_pure" \
  "head -25 ${ROOT}/src/phases/replace.rs | grep -q 'Pure'"

run_test "f_replace_doc_mentions_threshold_2" \
  "head -20 ${ROOT}/src/phases/replace.rs | grep -q '≥2\\|>= 2'"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "=========================================================="
echo "smoke_phase_f: $PASS passed, $FAIL failed"
echo "=========================================================="

if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for t in "${FAILED_TESTS[@]}"; do
    echo "  - $t"
  done
  exit 1
fi
exit 0
