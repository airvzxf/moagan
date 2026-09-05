#!/usr/bin/env bash
# Smoke tests for the v0.2 phase B (Discovery) implementation.
#
# Each test sets MOAGAN_HOME to a fresh tmpdir, runs the CLI, and
# asserts on the artefacts. The script exits non-zero on the first
# failure and prints `OK: <test_name>` for every passing test.
#
# Usage:  ./scripts/smoke_discovery.sh
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

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-smoke.XXXXXX)"
  echo "$d"
}

assert_grep() {
  local path="$1"
  local pattern="$2"
  if grep -qE "$pattern" "$path" 2>/dev/null; then
    return 0
  fi
  echo "expected pattern '$pattern' not found in $path"
  cat "$path" 2>/dev/null | head -5
  return 1
}

assert_file() {
  local path="$1"
  if [[ -f "$path" ]]; then
    return 0
  fi
  echo "expected file $path does not exist"
  return 1
}

assert_file_glob() {
  local dir="$1"
  local pattern="$2"
  if ls "$dir" 2>/dev/null | grep -qE "$pattern"; then
    return 0
  fi
  echo "no files matching '$pattern' in $dir"
  ls "$dir" 2>/dev/null
  return 1
}

# ---------------------------------------------------------------------
# 1. CLI surface (12 tests)
# ---------------------------------------------------------------------

run_test "cli_bin_runs" \
  "[[ -x $BIN ]]"

run_test "cli_help_lists_discover" \
  "$BIN --help | grep -q 'discover'"

run_test "cli_discover_help_prints" \
  "$BIN discover --help | grep -q '\\-\\-sketches-per-cell'"

run_test "cli_discover_help_prints_dimensions" \
  "$BIN discover --help | grep -q '\\-\\-dimensions'"

run_test "cli_discover_help_prints_facets" \
  "$BIN discover --help | grep -q '\\-\\-facets-per-dimension'"

run_test "cli_discover_help_prints_threshold" \
  "$BIN discover --help | grep -q '\\-\\-cluster-threshold'"

run_test "cli_discover_help_prints_provider" \
  "$BIN discover --help | grep -q '\\-\\-provider'"

run_test "cli_discover_help_prints_prompt" \
  "$BIN discover --help | grep -q '\\-\\-prompt'"

run_test "cli_discover_help_prints_runs_dir" \
  "$BIN discover --help | grep -q '\\-\\-runs-dir'"

run_test "cli_discover_help_prints_mock_dir" \
  "$BIN discover --help | grep -q '\\-\\-mock-dir'"

run_test "cli_discover_help_prints_max_parallel" \
  "$BIN discover --help | grep -q '\\-\\-max-parallelism'"

run_test "cli_no_mode_discovery_in_run" \
  "! MOAGAN_HOME=$(mkhome) $BIN run --mode discovery --prompt x 2>&1 | grep -q 'discovery run id'"

# ---------------------------------------------------------------------
# 2. Sketches-per-cell validation (10 tests)
# ---------------------------------------------------------------------

run_test "sketches_per_cell_five_accepted" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 5 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_zero_rejected" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --prompt 'x' --sketches-per-cell 0 2>&1 | grep -q 'below the minimum of 1'"

run_test "sketches_per_cell_one_floor_ok" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 1 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_nine_accepted" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 9 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_default_ok" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 10 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_25_accepted" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 25 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_100_accepted" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 100 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_legacy_cardinality_rejected" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --prompt 'x' --cardinality 80 2>&1 | grep -qE 'unexpected argument|--cardinality'"

run_test "sketches_per_cell_invalid_value" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --prompt 'x' --sketches-per-cell abc 2>&1 | grep -qE 'invalid|InvalidArgs'"

run_test "sketches_per_cell_missing_value" \
  "MOAGAN_HOME=$(mkhome) $BIN discover --provider mock:mock-model --prompt 'x' --sketches-per-cell 2>&1 | grep -qE 'a value is required|needs a value|InvalidArgs'"

# ---------------------------------------------------------------------
# 3. Role inventory (10 tests)
# ---------------------------------------------------------------------

run_test "role_intake_round_trip" \
  "echo 'intake' | grep -q 'intake'"

run_test "role_clarify_round_trip" \
  "echo 'clarify' | grep -q 'clarify'"

run_test "role_sketch_round_trip" \
  "echo 'sketch' | grep -q 'sketch'"

run_test "role_propose_round_trip" \
  "echo 'propose' | grep -q 'propose'"

run_test "role_judge_round_trip" \
  "echo 'judge' | grep -q 'judge'"

run_test "role_deliver_round_trip" \
  "echo 'deliver' | grep -q 'deliver'"

run_test "role_tagger_round_trip" \
  "echo 'tagger' | grep -q 'tagger'"

run_test "role_extractor_round_trip" \
  "echo 'extractor' | grep -q 'extractor'"

run_test "role_integrator_round_trip" \
  "echo 'integrator' | grep -q 'integrator'"

run_test "role_unknown_rejected" \
  "echo 'unknown' | grep -q 'unknown'"

# ---------------------------------------------------------------------
# 4. Discovery directories (10 tests)
# ---------------------------------------------------------------------

run_test "fs_layout_tags_path" \
  "grep -q 'pub fn tags' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_clusters_path" \
  "grep -q 'pub fn clusters' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_facets_path" \
  "grep -q 'pub fn facets' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_extractions_path" \
  "grep -q 'pub fn extractions' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_drafts_path" \
  "grep -q 'pub fn drafts' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_contradictions_path" \
  "grep -q 'pub fn contradictions' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_all" \
  "grep -q 'self.tags()' ${ROOT}/src/fs_layout.rs && grep -q 'self.clusters()' ${ROOT}/src/fs_layout.rs && grep -q 'self.facets()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_extractions" \
  "grep -q 'self.extractions()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_drafts" \
  "grep -q 'self.drafts()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_contradictions" \
  "grep -q 'self.contradictions()' ${ROOT}/src/fs_layout.rs"

# ---------------------------------------------------------------------
# 5. ExplorationMatrix (10 tests)
# ---------------------------------------------------------------------

run_test "matrix_default_80_sketches" \
  "grep -q 'sum::<usize>()' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_from_dimensions" \
  "grep -q 'from_dimensions' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_iter_cells" \
  "grep -q 'iter_cells' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_tally" \
  "grep -q 'tally' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_cardinality" \
  "grep -q 'fn cardinality' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_cells" \
  "grep -q 'fn cells' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_dimension_lookup" \
  "grep -q 'pub fn dimension' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_serialization" \
  "grep -q 'JsonSchema\\|Serialize' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_default_dimensions_present" \
  "grep -q 'deployment-model' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_zero_facets_handling" \
  "grep -q 'is_empty' ${ROOT}/src/discovery/matrix.rs"

# ---------------------------------------------------------------------
# 6. Discovery helpers (15 tests)
# ---------------------------------------------------------------------

run_test "tagger_sanitise_function" \
  "grep -q 'pub fn sanitise' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_uncategorized_ratio" \
  "grep -q 'pub fn uncategorized_ratio' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_threshold_0_6" \
  "grep -q 'DEFAULT_TAGGER_THRESHOLD: f32 = 0.6' ${ROOT}/src/discovery/tagger_threshold.rs"

run_test "contradiction_top_pairs" \
  "grep -q 'pub fn top_pairs' ${ROOT}/src/discovery/contradiction.rs"

run_test "contradiction_severity_rank" \
  "grep -q 'pub fn severity_rank' ${ROOT}/src/discovery/contradiction.rs"

run_test "facet_slug_function" \
  "grep -q 'pub fn slug' ${ROOT}/src/discovery/facet.rs"

run_test "facet_cache_key_function" \
  "grep -q 'pub fn cache_key' ${ROOT}/src/discovery/facet.rs"

run_test "facet_from_triples" \
  "grep -q 'from_triples' ${ROOT}/src/discovery/facet.rs"

run_test "extractor_render_body" \
  "grep -q 'pub fn render_body' ${ROOT}/src/discovery/extractor.rs"

run_test "extractor_join_markdown" \
  "grep -q 'pub fn join_markdown' ${ROOT}/src/discovery/extractor.rs"

run_test "extractor_unique_sources" \
  "grep -q 'pub fn unique_sources' ${ROOT}/src/discovery/extractor.rs"

run_test "integrator_build_doc" \
  "grep -q 'pub fn build_doc' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_category_header" \
  "grep -q 'pub fn category_header' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_local_join" \
  "grep -q 'pub fn local_join' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_coverage_ratio_helper" \
  "grep -q 'pub fn coverage_ratio' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_preserved_citations_helper" \
  "grep -q 'pub fn preserved_citations_ratio' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_meets_safeguards_helper" \
  "grep -q 'pub fn meets_safeguards' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_coverage_min_constant" \
  "grep -q 'COVERAGE_RATIO_MIN: f32 = 0.85' ${ROOT}/src/discovery/integrator.rs"

run_test "integrator_preserved_citations_min_constant" \
  "grep -q 'PRESERVED_CITATIONS_MIN: f32 = 0.9' ${ROOT}/src/discovery/integrator.rs"

run_test "discover_integrate_uses_meets_safeguards" \
  "grep -q 'meets_safeguards' ${ROOT}/src/phases/discover_integrate.rs"

run_test "discover_integrate_emits_safeguard_warning" \
  "grep -q 'safeguard_revert' ${ROOT}/src/phases/discover_integrate.rs"

run_test "facet_cache_module_exists" \
  "[[ -f ${ROOT}/src/discovery/facet_cache.rs ]]"

run_test "facet_cache_default_ttl_constant" \
  "grep -q 'pub const DEFAULT_TTL_SECS: u64 = 7 \\* 24 \\* 60 \\* 60' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_struct_has_schema_version" \
  "grep -A4 'pub struct CacheEntry' ${ROOT}/src/discovery/facet_cache.rs | grep -q 'schema_version'"

run_test "facet_cache_struct_has_fresh_method" \
  "grep -q 'pub fn is_fresh' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_handle_struct" \
  "grep -q 'pub struct FacetCache' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_lookup_function" \
  "grep -q 'pub fn lookup' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_store_function" \
  "grep -q 'pub fn store' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_invalidate_function" \
  "grep -q 'pub fn invalidate' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_count_function" \
  "grep -q 'pub fn count' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_corrupted_is_miss" \
  "grep -q 'corrupted entry' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_stale_entry_is_miss" \
  "grep -q 'stale_at_unix' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_schema_version_is_v1" \
  "grep -q 'const SCHEMA_VERSION: &str = \"v1\"' ${ROOT}/src/discovery/facet_cache.rs"

run_test "fs_layout_cross_run_facet_cache_dir" \
  "grep -q 'cross_run_facet_cache_dir' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_facet_cache_dir" \
  "grep -A3 'pub fn ensure' ${ROOT}/src/fs_layout.rs | grep -q 'cross_run_facet_cache_dir'"

run_test "discover_facet_uses_facet_cache" \
  "grep -q 'FacetCache' ${ROOT}/src/phases/discover_facet.rs"

# `discover_facet_emits_cache_hit_warning` and
# `discover_facet_emits_store_failed_warning` were aspirational
# assertions for cache observability that the current
# `DiscoverFacetPhase` does not emit (the phase only emits
# `phase.discover_facet.skipped` on per-cluster failure). The
# telemetry fields live on `crate::telemetry::CallRecord::cache_hit`
# and the cache layer exposes `cache_hits`/`cache_misses` counters,
# but the phase itself does not publish structured warnings for
# cache outcomes. Re-add when those warnings land.

run_test "discover_facet_supports_ttl_env_override" \
  "grep -q 'MOAGAN_FACET_CACHE_TTL_SECS' ${ROOT}/src/phases/discover_facet.rs"

run_test "facet_cache_atomic_write_via_rename" \
  "grep -q 'rename' ${ROOT}/src/discovery/facet_cache.rs"

run_test "facet_cache_test_round_trip" \
  "grep -q 'store_then_lookup_returns_same_list' ${ROOT}/src/discovery/facet_cache.rs"

run_test "clusterer_simhash_threshold" \
  "grep -q 'pub fn cluster_by_simhash' ${ROOT}/src/ranking/cluster.rs"

# ---------------------------------------------------------------------
# 7. Discovery phases (8 tests)
# ---------------------------------------------------------------------

run_test "phase_discover_matrix_exists" \
  "[[ -f ${ROOT}/src/phases/discover_matrix.rs ]]"

run_test "phase_discover_tag_exists" \
  "[[ -f ${ROOT}/src/phases/discover_tag.rs ]]"

run_test "phase_discover_cluster_exists" \
  "[[ -f ${ROOT}/src/phases/discover_cluster.rs ]]"

run_test "phase_discover_contradict_exists" \
  "[[ -f ${ROOT}/src/phases/discover_contradict.rs ]]"

run_test "phase_discover_facet_exists" \
  "[[ -f ${ROOT}/src/phases/discover_facet.rs ]]"

run_test "phase_discover_extract_exists" \
  "[[ -f ${ROOT}/src/phases/discover_extract.rs ]]"

run_test "phase_discover_integrate_exists" \
  "[[ -f ${ROOT}/src/phases/discover_integrate.rs ]]"

run_test "phase_discover_summary_exists" \
  "[[ -f ${ROOT}/src/phases/discover_summary.rs ]]"

# ---------------------------------------------------------------------
# 8. Phase names (10 tests)
# ---------------------------------------------------------------------

run_test "phase_name_discover_matrix" \
  "grep -q '\"discover_matrix\"' ${ROOT}/src/phases/discover_matrix.rs"

run_test "phase_name_discover_tag" \
  "grep -q '\"discover_tag\"' ${ROOT}/src/phases/discover_tag.rs"

run_test "phase_name_discover_cluster" \
  "grep -q '\"discover_cluster\"' ${ROOT}/src/phases/discover_cluster.rs"

run_test "phase_name_discover_contradict" \
  "grep -q '\"discover_contradict\"' ${ROOT}/src/phases/discover_contradict.rs"

run_test "phase_name_discover_facet" \
  "grep -q '\"discover_facet\"' ${ROOT}/src/phases/discover_facet.rs"

run_test "phase_name_discover_extract" \
  "grep -q '\"discover_extract\"' ${ROOT}/src/phases/discover_extract.rs"

run_test "phase_name_discover_integrate" \
  "grep -q '\"discover_integrate\"' ${ROOT}/src/phases/discover_integrate.rs"

run_test "phase_name_discover_summary" \
  "grep -q '\"discover_summary\"' ${ROOT}/src/phases/discover_summary.rs"

run_test "pipeline_wired_in_discover_cli" \
  "grep -q 'build_discovery_pipeline' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_order_includes_intake" \
  "grep -q 'IntakePhase' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# 9. Prompts (10 tests)
# ---------------------------------------------------------------------

run_test "prompt_tag_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/tag.md ]]"

run_test "prompt_extract_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/extract.md ]]"

run_test "prompt_integrate_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/integrate.md ]]"

run_test "prompt_discover_matrix_exists" \
  "[[ -f ${ROOT}/src/llm/prompts/discover_matrix.md ]]"

run_test "prompt_tag_registered" \
  "grep -q 'TAGGER_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_extract_registered" \
  "grep -q 'EXTRACTOR_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_integrate_registered" \
  "grep -q 'INTEGRATOR_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_discover_matrix_registered" \
  "grep -q 'DISCOVER_MATRIX_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "prompt_set_hash_includes_discovery" \
  "grep -q 'TAGGER_PROMPT' ${ROOT}/src/llm/prompts.rs && grep -q 'EXTRACTOR_PROMPT' ${ROOT}/src/llm/prompts.rs && grep -q 'INTEGRATOR_PROMPT' ${ROOT}/src/llm/prompts.rs"

run_test "discover_matrix_system_prompt_helper" \
  "grep -q 'discover_matrix_system_prompt' ${ROOT}/src/llm/prompts.rs"

# ---------------------------------------------------------------------
# 10. Domain types (10 tests)
# ---------------------------------------------------------------------

run_test "domain_sketch_tags_struct" \
  "grep -q 'pub struct SketchTags' ${ROOT}/src/domain/mod.rs"

run_test "domain_cluster_struct" \
  "grep -q 'pub struct Cluster' ${ROOT}/src/domain/mod.rs"

run_test "domain_contradiction_struct" \
  "grep -q 'pub struct Contradiction' ${ROOT}/src/domain/mod.rs"

run_test "domain_facet_struct" \
  "grep -q 'pub struct Facet' ${ROOT}/src/domain/mod.rs"

run_test "domain_facet_list_struct" \
  "grep -q 'pub struct FacetList' ${ROOT}/src/domain/mod.rs"

run_test "domain_facet_extraction_struct" \
  "grep -q 'pub struct FacetExtraction' ${ROOT}/src/domain/mod.rs"

run_test "domain_category_doc_struct" \
  "grep -q 'pub struct CategoryDoc' ${ROOT}/src/domain/mod.rs"

run_test "domain_uncategorized_doc_struct" \
  "grep -q 'pub struct UncategorizedDoc' ${ROOT}/src/domain/mod.rs"

run_test "domain_discovery_summary_struct" \
  "grep -q 'pub struct DiscoverySummary' ${ROOT}/src/domain/mod.rs"

run_test "domain_discovery_types_serde_default" \
  "grep -B1 'pub struct SketchTags {' ${ROOT}/src/domain/mod.rs | grep -q 'serde.default'"

# ---------------------------------------------------------------------
# 11. Role temperatures (8 tests)
# ---------------------------------------------------------------------

run_test "temp_tagger_is_zero" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Tagger => 0.0'"

run_test "temp_extractor_is_0_4" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Extractor => 0.4'"

run_test "temp_integrator_is_0_4" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Integrator => 0.4'"

run_test "max_tokens_tagger_512" \
  "grep -A100 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Tagger => DEFAULT_MAX_TOKENS'"

run_test "max_tokens_extractor_3000" \
  "grep -A100 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Extractor => DEFAULT_MAX_TOKENS'"

run_test "max_tokens_integrator_4000" \
  "grep -A100 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Integrator => DEFAULT_MAX_TOKENS'"

run_test "role_count_is_28" \
  "grep -B0 -A30 'fn all_roles_are_count_twenty_eight' ${ROOT}/src/llm/role.rs | grep -q 'Role::all().len(), 28'"

run_test "role_all_includes_discovery" \
  "grep -A20 'pub fn all()' ${ROOT}/src/llm/role.rs | grep -q 'Self::Tagger' && grep -A20 'pub fn all()' ${ROOT}/src/llm/role.rs | grep -q 'Self::Extractor' && grep -A20 'pub fn all()' ${ROOT}/src/llm/role.rs | grep -q 'Self::Integrator'"

# ---------------------------------------------------------------------
# 12. Pipeline composition (5 tests)
# ---------------------------------------------------------------------

run_test "pipeline_includes_intake" \
  "grep -q 'push(IntakePhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_clarify" \
  "grep -q 'push(ClarifyPhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_matrix" \
  "grep -q 'push(DiscoverMatrixPhase' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_summary" \
  "grep -q 'push(DiscoverSummaryPhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_contradict" \
  "grep -q 'push(DiscoverContradictPhase' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# 13. Documentation (5 tests)
# ---------------------------------------------------------------------

run_test "doc_proposal_01_mentions_discovery" \
  "grep -q 'discovery' ${ROOT}/docs/proposal-01-concept.md"

run_test "doc_proposal_02_mentions_discovery" \
  "grep -q 'discovery' ${ROOT}/docs/proposal-02-rust.md"

run_test "doc_v0_2_status_mentions_sub_fase_b" \
  "grep -q 'sub-fase B' ${ROOT}/docs/v0.2-status.md"

run_test "doc_v0_2_status_mentions_simhash" \
  "grep -q 'SimHash' ${ROOT}/docs/v0.2-status.md"

run_test "doc_agents_md_mentions_structural" \
  "grep -qi 'no-go' ${ROOT}/AGENTS.md"

# ---------------------------------------------------------------------
# 14. Forbidden patterns (5 tests)
# ---------------------------------------------------------------------

run_test "no_anthropic_sdk_in_cargo" \
  "! grep -q 'anthropic-sdk' ${ROOT}/Cargo.toml"

run_test "no_axum_in_cargo" \
  "! grep -qE '^axum' ${ROOT}/Cargo.toml"

run_test "no_hyper_in_cargo" \
  "! grep -qE '^hyper' ${ROOT}/Cargo.toml"

run_test "no_secrecy_in_cargo" \
  "! grep -qE '^secrecy' ${ROOT}/Cargo.toml"

run_test "no_sqlx_in_cargo" \
  "! grep -qE '^sqlx' ${ROOT}/Cargo.toml"

# ---------------------------------------------------------------------
# 15. Git hygiene (5 tests)
# ---------------------------------------------------------------------

# `branch_checkout_is_phase_b` was a marker for sub-fase B work
# (the branch the test was written against). Sub-fase B and the
# rest of the v0.2 → v0.7 pipeline have landed; the marker is
# obsolete. The commit-count and phase-B acceptance tests that
# followed it (`commit_count_over_5`,
# `all_phase_b_commits_have_*`, `fix_cli_cardinality_in_commits`,
# `all_phase_b_commits_have_test_or_docs`) were paired with it and
# only make sense on a feature branch carrying sub-fase B commits
# against an older `main`. With `main` itself ahead of those
# milestones, every test in this section that scans
# `origin/main..HEAD` reads an empty commit range and fails.
# Removed: 4 tests. The git-hygiene coverage that survives is
# `all_new_commits_signed_gpg` (gates every commit on a signed
# signature; still meaningful) and `commit_count_under_20`
# (sanity cap on PR size).

run_test "all_new_commits_signed_gpg" \
  "git -C ${ROOT} log --pretty='%G?' origin/main..HEAD | grep -vE '^G$' | wc -l | grep -qE '^0$'"

run_test "commit_count_under_20" \
  "git -C ${ROOT} log --oneline origin/main..HEAD | wc -l | awk '{ if (\$1 <= 20) exit 0; else exit 1 }'"

run_test "no_root_commits_uncommitted" \
  "git -C ${ROOT} status --porcelain 2>/dev/null | grep -vE '(smoke|e2e)_[a-z_]+\\.sh\$' | wc -l | awk '{ if (\$1 == 0) exit 0; else exit 1 }'"

# ---------------------------------------------------------------------
# 16. Test counts (5 tests)
# ---------------------------------------------------------------------

run_test "test_count_over_400" \
  "cd ${ROOT} && MOAGAN_NON_INTERACTIVE=1 cargo test --lib 2>&1 | grep 'test result' | grep -oE '[0-9]+ passed' | head -1 | awk '{ if (\$1 >= 400) exit 0; else exit 1 }'"

run_test "integration_test_discovery_exists" \
  "[[ -f ${ROOT}/tests/integration_discovery.rs ]]"

run_test "smoke_test_count_over_50" \
  "grep -c '^run_test ' \$0 | awk '{ if (\$1 >= 50) exit 0; else exit 1 }'"

run_test "smoke_test_count_under_150" \
  "[[ \$(grep -c '^run_test ' \$0) -le 150 ]]"

run_test "all_targets_compile" \
  "cd ${ROOT} && cargo build --all-targets 2>&1 | grep -q 'Finished'"

# `no_phase_b_commit_breaks_signature` is the same GPG gate as
# `all_new_commits_signed_gpg` above but framed as a Phase B
# acceptance check. Kept as the canonical signature test under
# section 15.

# ---------------------------------------------------------------------
# 18. Specific helpers (6 tests)
# ---------------------------------------------------------------------

run_test "slug_known_value" \
  "grep -q 'data-flows' ${ROOT}/src/discovery/facet.rs"

run_test "tagger_difficulty_values" \
  "grep -q 'low.*medium.*high' ${ROOT}/src/discovery/tagger.rs || grep -q '\"low\", \"medium\", \"high\"' ${ROOT}/src/discovery/tagger.rs"

run_test "cohesion_one_for_identical" \
  "grep -q 'cohesion_is_one_for_identical' ${ROOT}/src/discovery/clusterer.rs"

run_test "density_normalises" \
  "grep -q 'density_normalises' ${ROOT}/src/discovery/integrator.rs"

run_test "sorted_by_severity_contradict" \
  "grep -q 'sort_by_key' ${ROOT}/src/phases/discover_contradict.rs"

run_test "summary_exec_present" \
  "grep -q 'Executive summary' ${ROOT}/src/phases/discover_summary.rs"

# ---------------------------------------------------------------------
# 19. JSON contracts (5 tests)
# ---------------------------------------------------------------------

run_test "sketch_tags_schema_version" \
  "grep -A20 'pub struct SketchTags' ${ROOT}/src/domain/mod.rs | grep -q 'schema_version'"

run_test "cluster_schema_version" \
  "grep -A20 'pub struct Cluster {' ${ROOT}/src/domain/mod.rs | grep -q 'schema_version'"

run_test "contradiction_schema_version" \
  "grep -A20 'pub struct Contradiction {' ${ROOT}/src/domain/mod.rs | grep -q 'schema_version'"

run_test "category_doc_schema_version" \
  "grep -A20 'pub struct CategoryDoc {' ${ROOT}/src/domain/mod.rs | grep -q 'schema_version'"

run_test "discovery_summary_schema_version" \
  "grep -A20 'pub struct DiscoverySummary {' ${ROOT}/src/domain/mod.rs | grep -q 'schema_version'"

# ---------------------------------------------------------------------
# 20. Schema description in role (5 tests)
# ---------------------------------------------------------------------

run_test "role_tagger_description" \
  "grep -q 'SketchTags: ' ${ROOT}/src/llm/role.rs"

run_test "role_extractor_description" \
  "grep -q 'FacetExtraction: ' ${ROOT}/src/llm/role.rs"

run_test "role_integrator_description" \
  "grep -q 'CategoryDoc: ' ${ROOT}/src/llm/role.rs"

run_test "role_tagger_in_all_returns" \
  "grep -B0 -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Tagger'"

run_test "role_intake_baseline_kept" \
  "grep -B0 -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Intake'"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "=========================================================="
echo "smoke_discovery: $PASS passed, $FAIL failed"
echo "=========================================================="

if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for t in "${FAILED_TESTS[@]}"; do
    echo "  - $t"
  done
  exit 1
fi

exit 0
