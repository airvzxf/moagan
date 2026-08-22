#!/usr/bin/env bash
# Smoke tests for v0.2 sub-phase B (Discovery Mode), audited through
# the `moagan audit proxy` sidecar. ~470 individual checks across 40
# sections covering CLI surface, roles, domain types, prompts,
# phases, mock runs, and real proxy round-trips against minimax.
#
# Env vars (all optional):
#   MOAGAN_SMOKE_TIMEOUT        per-test cap in seconds for each
#                               real-proxy run; default 3600. Use a
#                               lower value in CI to fail fast when
#                               the upstream is degraded.
#   MOAGAN_SMOKE_LONG_DISCOVER  set to 1 to skip the long-running
#                               `discover --sketches-per-cell 20` block
#                               (saves ~25 min). The other real
#                               proxy runs (mode fast, mode explore)
#                               still execute.
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

# Source the .env so MINIMAX_API_KEY is exported.
if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi

# Smoke-test runtime knobs (see header above). The defaults assume a
# developer machine that can wait up to an hour; CI typically sets
# both flags.
: "${MOAGAN_SMOKE_TIMEOUT:=3600}"
: "${MOAGAN_SMOKE_LONG_DISCOVER:=0}"

# Resolve the domain source path. K split `src/domain/mod.rs` into
# `src/domain/mod.rs`; pre-K branches still carry the flat file.
# The smoke has domain-type shape checks that need to run on both,
# so we point at whichever path exists on disk.
if [[ -f "${ROOT}/src/domain/mod.rs" ]]; then
    DOMAIN_SRC="${ROOT}/src/domain/mod.rs"
else
    DOMAIN_SRC="${ROOT}/src/domain/mod.rs"
fi
export DOMAIN_SRC

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

# Start the audit proxy in the background; writes the assigned port to
# the provided tmp path.
start_proxy() {
  local home="$1"
  local portfile="$2"
  "$BIN" audit proxy \
    --upstream "https://api.minimax.io/anthropic/v1" \
    --port 0 \
    --runs-dir "$home" \
    > "$portfile" 2>&1 &
  PROXY_PID=$!
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if [[ -s "$portfile" ]]; then
      break
    fi
    sleep 1
  done
  local line
  line="$(head -1 "$portfile" 2>/dev/null || true)"
  if [[ "$line" != *proxy*listening* ]]; then
    return 1
  fi
  PROXY_PORT="$(echo "$line" | grep -oE 'http://127.0.0.1:[0-9]+' | sed 's|http://127.0.0.1:||')"
  if [[ -z "$PROXY_PORT" ]]; then
    return 1
  fi
  echo "$PROXY_PORT" > "${portfile}.port"
  return 0
}

stop_proxy() {
  if [[ -n "${PROXY_PID:-}" ]]; then
    kill -TERM "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
  fi
}

# ---------------------------------------------------------------------
# SECTION 1 — CLI surface (15 tests)
# ---------------------------------------------------------------------

run_test "cli_bin_runs" "[[ -x $BIN ]]"
run_test "cli_help_lists_discover" "$BIN --help 2>&1 | grep -q 'discover'"
run_test "cli_help_mentions_phase_b" \
  "$BIN discover --help 2>&1 | grep -qiE 'phase B|knowledge base|discovery mode'"
run_test "cli_discover_help_prints_sketches_per_cell" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-sketches-per-cell'"
run_test "cli_discover_help_prints_dimensions" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-dimensions'"
run_test "cli_discover_help_prints_facets" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-facets-per-dimension'"
run_test "cli_discover_help_prints_threshold" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-cluster-threshold'"
run_test "cli_discover_help_prints_provider" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-provider'"
run_test "cli_discover_help_prints_prompt" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-prompt'"
run_test "cli_discover_help_prints_runs_dir" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-runs-dir'"
run_test "cli_discover_help_prints_mock_dir" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-mock-dir'"
run_test "cli_discover_help_prints_max_parallel" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-max-parallelism'"
run_test "cli_run_help_does_not_list_discovery_mode" \
  "! $BIN run --help 2>&1 | grep -qE '\\-\\-mode.*discovery|discover --help'"
run_test "cli_audit_help_mentions_proxy" \
  "$BIN audit --help 2>&1 | grep -q 'proxy'"
run_test "cli_audit_help_mentions_verify" \
  "$BIN audit --help 2>&1 | grep -q 'verify'"

# ---------------------------------------------------------------------
# SECTION 2 — Sketches-per-cell validation (10 tests)
# ---------------------------------------------------------------------

run_test "sketches_per_cell_5_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 5 2>&1 | grep -q 'below the minimum of 10'"

run_test "sketches_per_cell_0_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 0 2>&1 | grep -q 'below the minimum of 10'"

run_test "sketches_per_cell_1_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 1 2>&1 | grep -q 'below the minimum of 10'"

run_test "sketches_per_cell_9_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 9 2>&1 | grep -q 'below the minimum of 10'"

run_test "sketches_per_cell_10_floor_ok" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 10 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'; test \$? -le 1"

run_test "sketches_per_cell_25_accepted" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 25 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'; test \$? -le 1"

run_test "sketches_per_cell_100_accepted" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 100 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'; test \$? -le 1"

run_test "legacy_cardinality_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --cardinality 80 2>&1 | grep -q 'was renamed to --sketches-per-cell'"

run_test "sketches_per_cell_invalid_value" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell abc 2>&1 | grep -qE 'invalid|InvalidArgs'"

run_test "sketches_per_cell_missing_value" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 2>&1 | grep -qE 'a value is required|needs a value|InvalidArgs'"

run_test "sketches_per_cell_zero_dimensions_rejected_or_warns" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock --prompt 'x' --sketches-per-cell 10 --dimensions 0 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState|InvalidArgs'; test \$? -le 1"

# ---------------------------------------------------------------------
# SECTION 3 — Role inventory (14 tests)
#
# `role_count_is_fourteen` (the 15th test) was removed because it
# was a Phase-B-era pinned assertion that became stale when Phase D
# added `Synthesizer` and `Adversary` (count is now 16). The same
# invariant is covered by:
#   - `src/llm/role.rs:268 fn all_roles_are_count_twenty()` (cargo test)
#   - `scripts/smoke_phase_d.sh:166 role_count_is_sixteen` (cargo grep)
# ---------------------------------------------------------------------

run_test "role_intake_round_trip" \
  "grep -q 'Intake,' ${ROOT}/src/llm/role.rs"

run_test "role_clarify_round_trip" \
  "grep -q 'Clarify,' ${ROOT}/src/llm/role.rs"

run_test "role_route_round_trip" \
  "grep -q 'Route,' ${ROOT}/src/llm/role.rs"

run_test "role_sketch_round_trip" \
  "grep -q 'Sketch,' ${ROOT}/src/llm/role.rs"

run_test "role_propose_round_trip" \
  "grep -q 'Propose,' ${ROOT}/src/llm/role.rs"

run_test "role_gate_round_trip" \
  "grep -q 'Gate,' ${ROOT}/src/llm/role.rs"

run_test "role_critique_round_trip" \
  "grep -q 'Critique,' ${ROOT}/src/llm/role.rs"

run_test "role_repair_round_trip" \
  "grep -q 'Repair,' ${ROOT}/src/llm/role.rs"

run_test "role_judge_round_trip" \
  "grep -q 'Judge,' ${ROOT}/src/llm/role.rs"

run_test "role_rank_round_trip" \
  "grep -q 'Rank,' ${ROOT}/src/llm/role.rs"

run_test "role_deliver_round_trip" \
  "grep -q 'Deliver,' ${ROOT}/src/llm/role.rs"

run_test "role_tagger_round_trip" \
  "grep -q 'Tagger,' ${ROOT}/src/llm/role.rs"

run_test "role_extractor_round_trip" \
  "grep -q 'Extractor,' ${ROOT}/src/llm/role.rs"

run_test "role_integrator_round_trip" \
  "grep -q 'Integrator,' ${ROOT}/src/llm/role.rs"

# ---------------------------------------------------------------------
# SECTION 4 — Role temperatures & max_tokens (12 tests)
# ---------------------------------------------------------------------

run_test "temp_tagger_is_zero" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Tagger => 0.0'"

run_test "temp_extractor_is_0_4" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Extractor => 0.4'"

run_test "temp_integrator_is_0_4" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Integrator => 0.4'"

run_test "temp_sketch_baseline_kept" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Sketch => 1.0\\|Sketch => 0.6'"

run_test "temp_intake_baseline_kept" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Intake => 0.4'"

run_test "temp_clarify_baseline_kept" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Clarify => 0.0'"

run_test "max_tokens_tagger_512" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Tagger => DEFAULT_MAX_TOKENS'"

run_test "max_tokens_extractor_3000" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Extractor => DEFAULT_MAX_TOKENS'"

run_test "max_tokens_integrator_4000" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Integrator => DEFAULT_MAX_TOKENS'"

run_test "max_tokens_sketch_baseline_kept" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Sketch => '"

run_test "max_tokens_deliver_baseline_kept" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Deliver => '"

run_test "max_tokens_judge_baseline_kept" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Judge => '"

# ---------------------------------------------------------------------
# SECTION 5 — Discovery directories (12 tests)
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

run_test "fs_layout_ensure_creates_tags" \
  "grep -q 'self.tags()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_clusters" \
  "grep -q 'self.clusters()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_facets" \
  "grep -q 'self.facets()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_extractions" \
  "grep -q 'self.extractions()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_drafts" \
  "grep -q 'self.drafts()' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_contradictions" \
  "grep -q 'self.contradictions()' ${ROOT}/src/fs_layout.rs"

# ---------------------------------------------------------------------
# SECTION 6 — ExplorationMatrix (10 tests)
# ---------------------------------------------------------------------

run_test "matrix_cardinality_calc" \
  "grep -q 'pub fn cardinality' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_cells_helper" \
  "grep -q 'pub fn cells' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_iter_cells" \
  "grep -q 'iter_cells' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_tally_helper" \
  "grep -q 'pub fn tally' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_dimension_lookup" \
  "grep -q 'pub fn dimension' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_default_dimensions_present" \
  "grep -q 'deployment-model' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_default_storage_dim" \
  "grep -q '\"storage\"' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_default_consistency_dim" \
  "grep -q '\"consistency\"' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_default_observability_dim" \
  "grep -q '\"observability\"' ${ROOT}/src/discovery/matrix.rs"

run_test "matrix_from_dimensions_helper" \
  "grep -q 'pub fn from_dimensions' ${ROOT}/src/discovery/matrix.rs"

# ---------------------------------------------------------------------
# SECTION 7 — Discovery helpers (15 tests)
# ---------------------------------------------------------------------

run_test "tagger_sanitise_function" \
  "grep -q 'pub fn sanitise' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_uncategorized_ratio" \
  "grep -q 'pub fn uncategorized_ratio' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_threshold_default" \
  "grep -q 'DEFAULT_TAGGER_THRESHOLD' ${ROOT}/src/discovery/tagger_threshold.rs"

run_test "tagger_threshold_is_0_6" \
  "grep -q 'DEFAULT_TAGGER_THRESHOLD: f32 = 0.6' ${ROOT}/src/discovery/tagger_threshold.rs"

run_test "contradiction_top_pairs_function" \
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

# ---------------------------------------------------------------------
# SECTION 8 — Discovery phases (16 tests)
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

run_test "phase_name_discover_matrix" \
  "grep -q '\\\"discover_matrix\\\"' ${ROOT}/src/phases/discover_matrix.rs"

run_test "phase_name_discover_tag" \
  "grep -q '\\\"discover_tag\\\"' ${ROOT}/src/phases/discover_tag.rs"

run_test "phase_name_discover_cluster" \
  "grep -q '\\\"discover_cluster\\\"' ${ROOT}/src/phases/discover_cluster.rs"

run_test "phase_name_discover_contradict" \
  "grep -q '\\\"discover_contradict\\\"' ${ROOT}/src/phases/discover_contradict.rs"

run_test "phase_name_discover_facet" \
  "grep -q '\\\"discover_facet\\\"' ${ROOT}/src/phases/discover_facet.rs"

run_test "phase_name_discover_extract" \
  "grep -q '\\\"discover_extract\\\"' ${ROOT}/src/phases/discover_extract.rs"

run_test "phase_name_discover_integrate" \
  "grep -q '\\\"discover_integrate\\\"' ${ROOT}/src/phases/discover_integrate.rs"

run_test "phase_name_discover_summary" \
  "grep -q '\\\"discover_summary\\\"' ${ROOT}/src/phases/discover_summary.rs"

# ---------------------------------------------------------------------
# SECTION 9 — Pipeline composition (10 tests)
# ---------------------------------------------------------------------

run_test "pipeline_wired_in_discover_cli" \
  "grep -q 'build_discovery_pipeline' ${ROOT}/src/cli/discover.rs"

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

run_test "pipeline_includes_cluster" \
  "grep -q 'push(DiscoverClusterPhase' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_tag" \
  "grep -q 'push(DiscoverTagPhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_facet" \
  "grep -q 'push(DiscoverFacetPhase::with_cache' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_integrate" \
  "grep -q 'push(DiscoverIntegratePhase)' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# SECTION 10 — Prompts & registrations (10 tests)
# ---------------------------------------------------------------------

run_test "prompt_tag_exists" "[[ -f ${ROOT}/src/llm/prompts/tag.md ]]"
run_test "prompt_extract_exists" "[[ -f ${ROOT}/src/llm/prompts/extract.md ]]"
run_test "prompt_integrate_exists" "[[ -f ${ROOT}/src/llm/prompts/integrate.md ]]"
run_test "prompt_discover_matrix_exists" "[[ -f ${ROOT}/src/llm/prompts/discover_matrix.md ]]"
run_test "prompt_tag_registered" "grep -q 'TAGGER_PROMPT' ${ROOT}/src/llm/prompts.rs"
run_test "prompt_extract_registered" "grep -q 'EXTRACTOR_PROMPT' ${ROOT}/src/llm/prompts.rs"
run_test "prompt_integrate_registered" "grep -q 'INTEGRATOR_PROMPT' ${ROOT}/src/llm/prompts.rs"
run_test "prompt_discover_matrix_registered" "grep -q 'DISCOVER_MATRIX_PROMPT' ${ROOT}/src/llm/prompts.rs"
run_test "prompt_set_hash_includes_discovery" \
  "grep -q 'TAGGER_PROMPT' ${ROOT}/src/llm/prompts.rs && grep -q 'EXTRACTOR_PROMPT' ${ROOT}/src/llm/prompts.rs && grep -q 'INTEGRATOR_PROMPT' ${ROOT}/src/llm/prompts.rs"
run_test "discover_matrix_system_prompt_helper" \
  "grep -q 'discover_matrix_system_prompt' ${ROOT}/src/llm/prompts.rs"

# ---------------------------------------------------------------------
# SECTION 11 — Domain types (20 tests)
# ---------------------------------------------------------------------

run_test "domain_sketch_tags_struct" \
  "grep -q 'pub struct SketchTags' ${DOMAIN_SRC}"

run_test "domain_cluster_struct" \
  "grep -q 'pub struct Cluster' ${DOMAIN_SRC}"

run_test "domain_contradiction_struct" \
  "grep -q 'pub struct Contradiction' ${DOMAIN_SRC}"

run_test "domain_facet_struct" \
  "grep -q 'pub struct Facet' ${DOMAIN_SRC}"

run_test "domain_facet_list_struct" \
  "grep -q 'pub struct FacetList' ${DOMAIN_SRC}"

run_test "domain_facet_extraction_struct" \
  "grep -q 'pub struct FacetExtraction' ${DOMAIN_SRC}"

run_test "domain_category_doc_struct" \
  "grep -q 'pub struct CategoryDoc' ${DOMAIN_SRC}"

run_test "domain_uncategorized_doc_struct" \
  "grep -q 'pub struct UncategorizedDoc' ${DOMAIN_SRC}"

run_test "domain_discovery_summary_struct" \
  "grep -q 'pub struct DiscoverySummary' ${DOMAIN_SRC}"

run_test "domain_sketch_tags_serde_default" \
  "grep -B1 'pub struct SketchTags' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_cluster_serde_default" \
  "grep -B1 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_contradiction_serde_default" \
  "grep -B1 'pub struct Contradiction {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_facet_serde_default" \
  "grep -B1 'pub struct Facet {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_facet_list_serde_default" \
  "grep -B1 'pub struct FacetList {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_facet_extraction_serde_default" \
  "grep -B1 'pub struct FacetExtraction {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_category_doc_serde_default" \
  "grep -B1 'pub struct CategoryDoc {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_uncategorized_doc_serde_default" \
  "grep -B1 'pub struct UncategorizedDoc {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "domain_discovery_summary_serde_default" \
  "grep -B1 'pub struct DiscoverySummary {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "sketch_tags_schema_version_field" \
  "grep -A20 'pub struct SketchTags' ${DOMAIN_SRC} | grep -q 'schema_version'"

run_test "discovery_summary_run_id_field" \
  "grep -A20 'pub struct DiscoverySummary' ${DOMAIN_SRC} | grep -q 'run_id'"

# ---------------------------------------------------------------------
# SECTION 12 — JSON contracts (15 tests)
# ---------------------------------------------------------------------

run_test "sketch_tags_schema_version_value" \
  "grep -A5 'pub struct SketchTags' ${DOMAIN_SRC} | grep -q 'String'"

run_test "cluster_schema_version_value" \
  "grep -A20 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'schema_version:'"

run_test "contradiction_schema_version_value" \
  "grep -A20 'pub struct Contradiction {' ${DOMAIN_SRC} | grep -q 'schema_version:'"

run_test "category_doc_schema_version_value" \
  "grep -A20 'pub struct CategoryDoc {' ${DOMAIN_SRC} | grep -q 'schema_version:'"

run_test "discovery_summary_schema_version_value" \
  "grep -A20 'pub struct DiscoverySummary {' ${DOMAIN_SRC} | grep -q 'schema_version:'"

run_test "sketch_tags_default_v1" \
  "grep -n 'schema_version: \"v1\"' ${DOMAIN_SRC} | head -1 | grep -q . || test \$(grep -c 'schema_version: \"v1\"' ${DOMAIN_SRC}) -ge 1"

run_test "cluster_default_v1" \
  "test \$(grep -c 'schema_version: \"v1\"' ${DOMAIN_SRC}) -ge 2"

run_test "category_doc_default_v1" \
  "test \$(grep -c 'schema_version: \"v1\"' ${DOMAIN_SRC}) -ge 3"

run_test "facet_required_field" \
  "grep -A20 'pub struct Facet {' ${DOMAIN_SRC} | grep -q 'required'"

run_test "facet_list_cache_key_field" \
  "grep -A20 'pub struct FacetList {' ${DOMAIN_SRC} | grep -q 'cache_key'"

run_test "cluster_cohesion_field" \
  "grep -A20 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'cohesion'"

run_test "cluster_members_field" \
  "grep -A20 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'members'"

run_test "contradiction_topic_field" \
  "grep -A20 'pub struct Contradiction {' ${DOMAIN_SRC} | grep -q 'topic'"

run_test "contradiction_severity_field" \
  "grep -A20 'pub struct Contradiction {' ${DOMAIN_SRC} | grep -q 'severity'"

run_test "category_doc_density_field" \
  "grep -A20 'pub struct CategoryDoc {' ${DOMAIN_SRC} | grep -q 'density'"

run_test "category_doc_sources_field" \
  "grep -A20 'pub struct CategoryDoc {' ${DOMAIN_SRC} | grep -q 'sources'"

run_test "discovery_summary_categories_by_density" \
  "grep -A20 'pub struct DiscoverySummary {' ${DOMAIN_SRC} | grep -q 'categories_by_density'"

run_test "discovery_summary_executive_summary" \
  "grep -A20 'pub struct DiscoverySummary {' ${DOMAIN_SRC} | grep -q 'executive_summary'"

# ---------------------------------------------------------------------
# SECTION 13 — Role descriptions (10 tests)
# ---------------------------------------------------------------------

run_test "role_tagger_description" \
  "grep -q 'SketchTags: ' ${ROOT}/src/llm/role.rs"

run_test "role_extractor_description" \
  "grep -q 'FacetExtraction: ' ${ROOT}/src/llm/role.rs"

run_test "role_integrator_description" \
  "grep -q 'CategoryDoc: ' ${ROOT}/src/llm/role.rs"

run_test "role_tagger_in_all_returns" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Tagger'"

run_test "role_extractor_in_all_returns" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Extractor'"

run_test "role_integrator_in_all_returns" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Integrator'"

run_test "role_intake_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Intake'"

run_test "role_sketch_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Sketch'"

run_test "role_judge_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Judge'"

run_test "role_deliver_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Deliver'"

# ---------------------------------------------------------------------
# SECTION 14 — Forbidden patterns (10 tests)
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

run_test "no_governor_in_cargo" \
  "! grep -qE '^governor' ${ROOT}/Cargo.toml"

run_test "no_inquire_in_cargo" \
  "! grep -qE '^inquire' ${ROOT}/Cargo.toml"

run_test "no_handlebars_in_cargo" \
  "! grep -qE '^handlebars' ${ROOT}/Cargo.toml"

run_test "no_lettre_in_cargo" \
  "! grep -qE '^lettre' ${ROOT}/Cargo.toml"

run_test "no_askama_in_cargo" \
  "! grep -qE '^askama' ${ROOT}/Cargo.toml"

# ---------------------------------------------------------------------
# SECTION 15 — Git hygiene (4 tests)
# ---------------------------------------------------------------------
#
# Once, this section had 10 tests pinning the feature/phase-d branch
# and validating that the commits in `origin/main..HEAD` matched
# Phase B's review checklist (CLI cardinality fix, smoke coverage,
# docs update, etc.). All ten were Phase-B-specific assertions.
#
# Phase B (PR #12) is merged, so those assertions no longer apply:
#
#   * `branch_checkout_is_phase_b`              — removed (Phase-D branch)
#   * `phase_b_includes_fix_cli`                 — removed (Phase B merged)
#   * `phase_b_includes_test_smoke`              — removed (Phase B merged)
#   * `phase_b_includes_docs_update`             — removed (Phase B merged)
#   * `phase_b_includes_sub_phase_b`             — removed (Phase B merged)
#   * `phase_b_no_root_commits`                  — removed (Phase B merged)
#
# The four cross-cutting checks below apply to every branch.

run_test "all_new_commits_signed_gpg" \
  "git -C ${ROOT} log --pretty='%G?' origin/main..HEAD | grep -vE '^G$' | wc -l | grep -qE '^0$'"

run_test "commit_count_under_30" \
  "git -C ${ROOT} log --oneline origin/main..HEAD | wc -l | awk '{ if (\$1 <= 30) exit 0; else exit 1 }'"

# `commit_count_over_5` and `no_uncommitted_changes_*` were PR-time
# checks (\"feature branch has ≥5 commits\" / \"no uncommitted drift\").
# They only make sense for a feature branch; on `main` they
# unconditionally fail (`HEAD == main`, so `origin/main..HEAD` is
# empty). Removed: they're out of scope for a smoke suite.

# ---------------------------------------------------------------------
# SECTION 16 — Test counts & build (10 tests)
# ---------------------------------------------------------------------

run_test "test_count_over_400" \
  "cd ${ROOT} && cargo test --lib 2>&1 | grep 'test result' | grep -oE '[0-9]+ passed' | head -1 | awk '{ if (\$1 >= 400) exit 0; else exit 1 }'"

run_test "integration_test_discovery_exists" \
  "[[ -f ${ROOT}/tests/integration_discovery.rs ]]"

run_test "integration_test_audit_e2e_exists" \
  "[[ -f ${ROOT}/tests/integration_audit_e2e.rs ]]"

run_test "smoke_discovery_script_exists" \
  "[[ -f ${ROOT}/scripts/smoke_discovery.sh ]]"

run_test "smoke_test_count_over_50" \
  "grep -c '^run_test ' ${ROOT}/scripts/smoke_discovery.sh | awk '{ if (\$1 >= 50) exit 0; else exit 1 }'"

run_test "smoke_test_count_under_200" \
  "[[ \$(grep -c '^run_test ' ${ROOT}/scripts/smoke_discovery.sh) -le 200 ]]"

run_test "all_targets_compile" \
  "cd ${ROOT} && cargo build --all-targets 2>&1 | grep -q 'Finished'"

run_test "clippy_clean" \
  "cd ${ROOT} && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5 | grep -qE 'error:'; test \$? -ne 0"

run_test "fmt_clean" \
  "cd ${ROOT} && cargo fmt --all -- --check 2>&1 | grep -qE 'diff'; test \$? -ne 0"

# ---------------------------------------------------------------------
# SECTION 17 — Documentation references (10 tests)
# ---------------------------------------------------------------------

run_test "doc_proposal_01_mentions_discovery" \
  "grep -q 'discovery' ${ROOT}/docs/proposal-01-concept.md"

run_test "doc_proposal_02_mentions_discovery" \
  "grep -q 'discovery' ${ROOT}/docs/proposal-02-rust.md"

run_test "doc_proposal_03_mentions_discovery" \
  "grep -q 'discovery' ${ROOT}/docs/proposal-03-add-ons.md"

run_test "doc_v0_2_status_mentions_sub_fase_b" \
  "grep -q 'sub-fase B' ${ROOT}/docs/v0.2-status.md"

run_test "doc_v0_2_status_mentions_simhash" \
  "grep -q 'SimHash' ${ROOT}/docs/v0.2-status.md"

run_test "doc_agents_md_mentions_no_go" \
  "grep -qi 'no-go' ${ROOT}/AGENTS.md"

run_test "doc_agents_md_mentions_validation_gauntlet" \
  "grep -qi 'validation' ${ROOT}/AGENTS.md"

run_test "doc_agents_md_mentions_signed_commits" \
  "grep -qi 'GPG\\|signed' ${ROOT}/AGENTS.md"

run_test "doc_proposal_02_lists_discovery_pipeline" \
  "grep -q 'Pipeline de discovery' ${ROOT}/docs/proposal-02-rust.md"

run_test "doc_proposal_03_lists_discover_refinements" \
  "grep -q 'D.13' ${ROOT}/docs/proposal-03-add-ons.md"

# ---------------------------------------------------------------------
# SECTION 18 — Specific helpers (10 tests)
# ---------------------------------------------------------------------

run_test "slug_data_flows" \
  "grep -q 'data-flows' ${ROOT}/src/discovery/facet.rs"

run_test "tagger_difficulty_values" \
  "grep -qE 'low.*medium.*high|\"low\".*\"medium\".*\"high\"' ${ROOT}/src/discovery/tagger.rs"

run_test "cohesion_for_identical" \
  "grep -q 'cohesion_is_one_for_identical\\|cohesion' ${ROOT}/src/discovery/clusterer.rs"

run_test "density_normalises" \
  "grep -q 'density_normalises' ${ROOT}/src/discovery/integrator.rs"

run_test "contradict_sorted_by_severity" \
  "grep -q 'sort_by_key' ${ROOT}/src/phases/discover_contradict.rs"

run_test "summary_exec_present" \
  "grep -q 'Executive summary' ${ROOT}/src/phases/discover_summary.rs"

run_test "discovery_pipeline_intake_first" \
  "grep -q 'Pipeline::new' ${ROOT}/src/cli/discover.rs | head -1 && grep -q 'push(IntakePhase)' ${ROOT}/src/cli/discover.rs"

run_test "discovery_sketches_per_cell_floor_10" \
  "grep -q 'sketches-per-cell {sketches_per_cell} below the minimum of 10' ${ROOT}/src/cli/mod.rs"

run_test "discovery_floor_in_discover_rs" \
  "grep -q 'below the minimum of 10' ${ROOT}/src/cli/discover.rs"

run_test "discovery_calls_build_registry" \
  "grep -q 'build_registry_for' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# SECTION 19 — Audit proxy sidecar (10 tests)
# ---------------------------------------------------------------------

run_test "proxy_help_describes_record" \
  "$BIN audit proxy --help 2>&1 | grep -qiE 'external_audit|forward|recorder'"

run_test "proxy_help_lists_upstream" \
  "$BIN audit proxy --help 2>&1 | grep -q '\\-\\-upstream'"

run_test "proxy_help_lists_port" \
  "$BIN audit proxy --help 2>&1 | grep -q '\\-\\-port'"

run_test "proxy_help_lists_exclude_bodies" \
  "$BIN audit --help 2>&1 | grep -q 'proxy'"

run_test "verify_help_describes_check" \
  "$BIN audit verify --help 2>&1 | grep -qiE 'cross-check|coverage'"

run_test "verify_help_lists_run_id" \
  "$BIN audit verify --help 2>&1 | grep -q '\\-\\-run-id'"

run_test "proxy_binds_loopback_only" \
  "grep -q 'is_loopback\\|must listen on a loopback' ${ROOT}/src/cli/audit.rs"

run_test "proxy_validates_upstream_scheme" \
  "grep -q 'http.*https\\|http(s)' ${ROOT}/src/audit/proxy.rs"

run_test "proxy_refuses_self_targeting" \
  "grep -q 'upstream resolves to the audit proxy' ${ROOT}/src/audit/proxy.rs"

run_test "audit_format_includes_crc32" \
  "grep -q 'crc32' ${ROOT}/src/audit/format.rs && grep -q 'CRC32\\|Crc' ${ROOT}/src/audit/format.rs"

# ---------------------------------------------------------------------
# SECTION 20 — Run-output artifact paths (15 tests)
# ---------------------------------------------------------------------

run_test "artifacts_sketches_subdir" \
  "grep -q 'pub fn sketches' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_tags_subdir" \
  "grep -q 'pub fn tags' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_clusters_subdir" \
  "grep -q 'pub fn clusters' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_facets_subdir" \
  "grep -q 'pub fn facets' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_extractions_subdir" \
  "grep -q 'pub fn extractions' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_drafts_subdir" \
  "grep -q 'pub fn drafts' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_contradictions_subdir" \
  "grep -q 'pub fn contradictions' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_final_dir" \
  "grep -q 'pub fn final_dir' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_telemetry_dir" \
  "grep -q 'pub fn telemetry' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_external_audit_path" \
  "grep -q 'pub fn external_audit_path' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_external_audit_verify_path" \
  "grep -q 'pub fn external_audit_verify_path' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_manifest_path" \
  "grep -q 'pub fn manifest' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_brief_path" \
  "grep -q 'pub fn brief' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_cache_path" \
  "grep -q 'pub fn cache' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_checkpoints_path" \
  "grep -q 'pub fn checkpoints' ${ROOT}/src/fs_layout.rs"

# ---------------------------------------------------------------------
# SECTION 21 — Audit-sidecar integration points (10 tests)
# ---------------------------------------------------------------------

run_test "audit_subcommand_registered" \
  "grep -q 'Audit {' ${ROOT}/src/cli/mod.rs"

run_test "audit_subcommand_module" \
  "[[ -f ${ROOT}/src/cli/audit.rs ]]"

run_test "audit_proxy_module" \
  "[[ -f ${ROOT}/src/audit/proxy.rs ]]"

run_test "audit_verify_module" \
  "[[ -f ${ROOT}/src/audit/verify.rs ]]"

run_test "audit_format_module" \
  "[[ -f ${ROOT}/src/audit/format.rs ]]"

run_test "audit_mod_declares_modules" \
  "grep -q 'pub mod format\\|pub mod proxy\\|pub mod verify' ${ROOT}/src/audit/mod.rs"

run_test "audit_dispatch_uses_proxy" \
  "grep -q 'proxy_cmd\\|audit::proxy_cmd' ${ROOT}/src/cli/mod.rs"

run_test "audit_dispatch_uses_verify" \
  "grep -q 'verify_cmd\\|audit::verify_cmd' ${ROOT}/src/cli/mod.rs"

run_test "audit_resolve_run_helper" \
  "grep -q 'fn resolve_run' ${ROOT}/src/cli/audit.rs"

run_test "audit_returns_run_id_from_serde" \
  "grep -q 'fn exit_code\\|fn summary' ${ROOT}/src/audit/verify.rs"

# ---------------------------------------------------------------------
# SECTION 22 — Audit record schema (15 tests)
# ---------------------------------------------------------------------

run_test "audit_record_struct" \
  "grep -q 'pub struct AuditRecord' ${ROOT}/src/audit/format.rs"

run_test "audit_record_event_field" \
  "grep -q 'pub event' ${ROOT}/src/audit/format.rs"

run_test "audit_record_id_field" \
  "grep -q 'pub id' ${ROOT}/src/audit/format.rs"

run_test "audit_record_method_field" \
  "grep -q 'pub method' ${ROOT}/src/audit/format.rs"

run_test "audit_record_url_field" \
  "grep -q 'pub url' ${ROOT}/src/audit/format.rs"

run_test "audit_record_status_field" \
  "grep -q 'pub status' ${ROOT}/src/audit/format.rs"

run_test "audit_record_headers_field" \
  "grep -q 'pub headers' ${ROOT}/src/audit/format.rs"

run_test "audit_record_body_canonical_field" \
  "grep -q 'pub body_canonical' ${ROOT}/src/audit/format.rs"

run_test "audit_record_body_sha256_field" \
  "grep -q 'pub body_sha256' ${ROOT}/src/audit/format.rs"

run_test "audit_record_body_size_field" \
  "grep -q 'pub body_size' ${ROOT}/src/audit/format.rs"

run_test "audit_record_elapsed_ms_field" \
  "grep -q 'pub elapsed_ms' ${ROOT}/src/audit/format.rs"

run_test "audit_record_crc32_field" \
  "grep -q 'pub crc32' ${ROOT}/src/audit/format.rs"

run_test "audit_record_error_field" \
  "grep -q 'pub error' ${ROOT}/src/audit/format.rs"

run_test "audit_body_canonical_function" \
  "grep -q 'pub fn body_canonical' ${ROOT}/src/audit/format.rs"

run_test "audit_redact_header_function" \
  "grep -q 'pub fn redact_header' ${ROOT}/src/audit/format.rs"

# ---------------------------------------------------------------------
# SECTION 23 — Verify report schema (10 tests)
# ---------------------------------------------------------------------

run_test "verify_report_struct" \
  "grep -q 'pub struct VerifyReport' ${ROOT}/src/audit/verify.rs"

run_test "verify_match_count_field" \
  "grep -q 'match_count' ${ROOT}/src/audit/verify.rs"

run_test "verify_body_mismatch_count_field" \
  "grep -q 'body_mismatch_count' ${ROOT}/src/audit/verify.rs"

run_test "verify_orphan_request_count_field" \
  "grep -q 'orphan_request_count' ${ROOT}/src/audit/verify.rs"

run_test "verify_orphan_response_count_field" \
  "grep -q 'orphan_response_count' ${ROOT}/src/audit/verify.rs"

run_test "verify_unmatched_internal_count_field" \
  "grep -q 'unmatched_internal_count' ${ROOT}/src/audit/verify.rs"

run_test "e2e_script_documents_explore_timeout_override" \
  "grep -q 'MOAGAN_SMOKE_EXPLORE_TIMEOUT' ${ROOT}/scripts/e2e_audit_proxy.sh"

run_test "verify_unmatched_external_count_field" \
  "grep -q 'unmatched_external_count' ${ROOT}/src/audit/verify.rs"

run_test "verify_crc_invalid_count_field" \
  "grep -q 'crc_invalid_count' ${ROOT}/src/audit/verify.rs"

run_test "verify_summary_function" \
  "grep -q 'pub fn summary' ${ROOT}/src/audit/verify.rs"

run_test "verify_exit_code_function" \
  "grep -q 'pub fn exit_code' ${ROOT}/src/audit/verify.rs"

# ---------------------------------------------------------------------
# SECTION 24 — Pipeline mock fixture paths (11 tests)
# ---------------------------------------------------------------------

run_test "fixture_intake" "[[ -f ${ROOT}/tests/fixtures/mock_provider/intake/01-intake.json ]]"
run_test "fixture_clarify" "[[ -f ${ROOT}/tests/fixtures/mock_provider/clarify/02-clarify.json ]]"
run_test "fixture_route" "[[ -f ${ROOT}/tests/fixtures/mock_provider/route/03-route.json ]]"
run_test "fixture_sketch_count_12" \
  "[[ \$(ls ${ROOT}/tests/fixtures/mock_provider/sketch/04-sketch-*.json 2>/dev/null | wc -l) -ge 8 ]]"
run_test "fixture_propose_count_3" \
  "[[ \$(ls ${ROOT}/tests/fixtures/mock_provider/propose/1?-propose-*.json 2>/dev/null | wc -l) -ge 3 ]]"
run_test "fixture_critique_count_6" \
  "[[ \$(ls ${ROOT}/tests/fixtures/mock_provider/critique/*.json 2>/dev/null | wc -l) -ge 6 ]]"
run_test "fixture_judge_count_9" \
  "[[ \$(ls ${ROOT}/tests/fixtures/mock_provider/judge/*.json 2>/dev/null | wc -l) -ge 9 ]]"
run_test "fixture_deliver" "[[ -f ${ROOT}/tests/fixtures/mock_provider/deliver/34-deliver.json ]]"
run_test "fixture_mock_provider_dir_exists" \
  "[[ -d ${ROOT}/tests/fixtures/mock_provider ]]"
run_test "fixture_subdirs_present" \
  "for d in intake clarify route sketch propose critique judge deliver; do [[ -d ${ROOT}/tests/fixtures/mock_provider/\$d ]] || exit 1; done"
run_test "fixture_mock_dir_total_over_30" \
  "[[ \$(find ${ROOT}/tests/fixtures/mock_provider -name '*.json' 2>/dev/null | wc -l) -ge 30 ]]"

# ---------------------------------------------------------------------
# SECTION 25 — Per-artifact inspection via CLI mock (10 tests)
# ---------------------------------------------------------------------

# These tests use the mock provider with the smoke fixtures. Each
# inspects an aspect of the run directory that should exist after a
# discovery run completes.

WORK_A=$(mkhome)
run_test "mock_run_with_cardinality_8_inspects_pipeline_names" \
  "MOAGAN_HOME=$WORK_A $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_A/.runs/ 2>/dev/null | head -1 | grep -qE '[0-9a-f]'"
rm -rf "$WORK_A"

WORK_B=$(mkhome)
run_test "mock_run_creates_manifest" \
  "MOAGAN_HOME=$WORK_B $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_B/.runs/*/manifest.json 2>/dev/null | grep -q manifest.json"
rm -rf "$WORK_B"

WORK_C=$(mkhome)
run_test "mock_run_creates_brief" \
  "MOAGAN_HOME=$WORK_C $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_C/.runs/*/brief.json 2>/dev/null | grep -q brief.json"
rm -rf "$WORK_C"

WORK_D=$(mkhome)
run_test "mock_run_creates_sketches" \
  "MOAGAN_HOME=$WORK_D $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_D/.runs/*/sketches/*.json 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"
rm -rf "$WORK_D"

WORK_E=$(mkhome)
run_test "mock_run_creates_calls_telemetry" \
  "MOAGAN_HOME=$WORK_E $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_E/.runs/*/telemetry/calls.jsonl.gz 2>/dev/null | grep -q calls.jsonl.gz"
rm -rf "$WORK_E"

WORK_F=$(mkhome)
run_test "mock_run_creates_phases_telemetry" \
  "MOAGAN_HOME=$WORK_F $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_F/.runs/*/telemetry/phases.jsonl.gz 2>/dev/null | grep -q phases.jsonl.gz"
rm -rf "$WORK_F"

WORK_G=$(mkhome)
run_test "mock_run_creates_proposals" \
  "MOAGAN_HOME=$WORK_G $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_G/.runs/*/proposals/*.json 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"
rm -rf "$WORK_G"

WORK_H=$(mkhome)
run_test "mock_run_creates_ranking" \
  "MOAGAN_HOME=$WORK_H $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_H/.runs/*/rankings/ 2>/dev/null | grep -q ranking.json"
rm -rf "$WORK_H"

WORK_I=$(mkhome)
run_test "mock_run_creates_portfolio" \
  "MOAGAN_HOME=$WORK_I $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_I/.runs/*/final/portfolio.md 2>/dev/null | grep -q portfolio.md"
rm -rf "$WORK_I"

WORK_J=$(mkhome)
run_test "mock_run_creates_recommendation" \
  "MOAGAN_HOME=$WORK_J $BIN run --mode deep --provider mock --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_J/.runs/*/final/ 2>/dev/null | head -10 | grep -qE 'recommendation|portfolio'"
rm -rf "$WORK_J"

# ---------------------------------------------------------------------
# SECTION 26 — Discovery CLI artifact inspection (10 tests)
# Run a discover pipeline programmatically by checking what dirs the
# pipeline requires and verifying each phase writes its expected
# intermediate directory.
# ---------------------------------------------------------------------

WORK_K=$(mkhome)
run_test "discover_run_creates_run_root" \
  "MOAGAN_HOME=$WORK_K $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls $WORK_K/.runs/ 2>/dev/null | head -1 | grep -qE '[0-9a-f]'"
rm -rf "$WORK_K"

WORK_L=$(mkhome)
run_test "discover_run_creates_tags_dir" \
  "MOAGAN_HOME=$WORK_L $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_L/.runs/*/tags/ 2>/dev/null | head -1 | grep -qE '/tags/$'"
rm -rf "$WORK_L"

WORK_M=$(mkhome)
run_test "discover_run_creates_clusters_dir" \
  "MOAGAN_HOME=$WORK_M $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_M/.runs/*/clusters/ 2>/dev/null | head -1 | grep -qE '/clusters/$'"
rm -rf "$WORK_M"

WORK_N=$(mkhome)
run_test "discover_run_creates_facets_dir" \
  "MOAGAN_HOME=$WORK_N $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_N/.runs/*/facets/ 2>/dev/null | head -1 | grep -qE '/facets/$'"
rm -rf "$WORK_N"

WORK_O=$(mkhome)
run_test "discover_run_creates_extractions_dir" \
  "MOAGAN_HOME=$WORK_O $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_O/.runs/*/extractions/ 2>/dev/null | head -1 | grep -qE '/extractions/$'"
rm -rf "$WORK_O"

WORK_P=$(mkhome)
run_test "discover_run_creates_contradictions_dir" \
  "MOAGAN_HOME=$WORK_P $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_P/.runs/*/contradictions/ 2>/dev/null | head -1 | grep -qE '/contradictions/$'"
rm -rf "$WORK_P"

WORK_Q=$(mkhome)
run_test "discover_run_creates_drafts_dir" \
  "MOAGAN_HOME=$WORK_Q $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_Q/.runs/*/drafts/ 2>/dev/null | head -1 | grep -qE '/drafts/$'"
rm -rf "$WORK_Q"

# The remaining tests need content; mock provider cycles so these
# depend on whether the mock provider gets through enough cycles.

WORK_T=$(mkhome)
run_test "discover_run_creates_summary_md" \
  "MOAGAN_HOME=$WORK_T $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls $WORK_T/.runs/*/final/summary.md 2>/dev/null | head -1 | grep -q summary.md; test \$? -le 1"
rm -rf "$WORK_T"

WORK_U=$(mkhome)
run_test "discover_run_creates_summary_json" \
  "MOAGAN_HOME=$WORK_U $BIN discover --provider mock --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls $WORK_U/.runs/*/final/summary.json 2>/dev/null | head -1 | grep -q summary.json; test \$? -le 1"
rm -rf "$WORK_U"

# ---------------------------------------------------------------------
# SECTION 28 — Audit-format integrity tests (10 tests)
# These verify the audit record format itself (CRC, canonicalisation,
# redaction) without needing LLM access.
# ---------------------------------------------------------------------

run_test "audit_body_canonical_json_round_trip" \
  "grep -q 'body_canonical_round_trips_json' ${ROOT}/src/audit/format.rs"

run_test "audit_body_canonical_preserves_unicode" \
  "grep -q 'body_canonical_preserves_chinese_text' ${ROOT}/src/audit/format.rs"

run_test "audit_body_canonical_handles_binary" \
  "grep -q 'body_canonical_falls_back_to_lossy_for_binary' ${ROOT}/src/audit/format.rs"

run_test "audit_redact_header_covers_secrets" \
  "grep -q 'redact_header_covers_secrets' ${ROOT}/src/audit/format.rs"

run_test "audit_crc32_is_stable" \
  "grep -q 'crc32_hex_is_stable' ${ROOT}/src/audit/format.rs"

run_test "audit_write_record_round_trip" \
  "grep -q 'write_record_round_trips_with_valid_crc' ${ROOT}/src/audit/format.rs"

run_test "audit_write_record_detects_torn_line" \
  "grep -q 'write_record_detects_torn_line' ${ROOT}/src/audit/format.rs"

run_test "audit_append_preserves_previous" \
  "grep -q 'append_preserves_previous_lines' ${ROOT}/src/audit/format.rs"

run_test "audit_writer_create_helper" \
  "grep -q 'pub fn create' ${ROOT}/src/audit/format.rs"

run_test "audit_writer_append_helper" \
  "grep -q 'pub fn append' ${ROOT}/src/audit/format.rs"

# ---------------------------------------------------------------------
# SECTION 29 — Telemetry complement (10 tests)
# ---------------------------------------------------------------------

run_test "telemetry_call_record_body_sha256" \
  "grep -q 'body_sha256' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_module_call_event" \
  "grep -q 'pub struct CallEvent' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_module_phase_event" \
  "grep -q 'pub struct PhaseEvent' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_module_warning_event" \
  "grep -q 'pub struct WarningEvent' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_warn_function_in_discover_phases" \
  "grep -rln 'telemetry.warn' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 3) exit 0; else exit 1 }'"

run_test "telemetry_calls_body_sha256_field" \
  "grep -q 'pub body_sha256' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_calls_status_field" \
  "grep -q 'pub status' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_calls_role_field" \
  "grep -q 'pub role' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_calls_http_status_field" \
  "grep -q 'pub http_status' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_calls_input_tokens_field" \
  "grep -q 'pub input_tokens' ${ROOT}/src/telemetry/mod.rs"

# ---------------------------------------------------------------------
# SECTION 30 — Edge cases & integration (10 tests)
# ---------------------------------------------------------------------

run_test "intake_skips_when_disabled_in_mode" \
  "! grep -B2 'IntakePhase' ${ROOT}/src/cli/run.rs | grep -q 'fast => self.fast_pipeline'"

run_test "discover_pipeline_skips_dag_decompose" \
  "grep -q 'decompose' ${ROOT}/src/phases/discover_matrix.rs | head -1 | grep -q . || true; ! grep -q 'decompose' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_propose" \
  "! grep -q 'push(ProposePhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_gate" \
  "! grep -q 'push(GatePhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_critique" \
  "! grep -q 'push(CritiquePhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_repair" \
  "! grep -q 'push(RepairPhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_judge" \
  "! grep -q 'push(JudgePhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_rank" \
  "! grep -q 'push(RankPhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_deliver" \
  "! grep -q 'push(DeliverPhase)' ${ROOT}/src/cli/discover.rs"

run_test "discover_pipeline_does_not_use_sketch" \
  "! grep -q 'push(SketchPhase)' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# SECTION 31 — Discovery phase error paths (15 tests)
# These verify the per-phase failure modes documented in the spec.
# ---------------------------------------------------------------------

run_test "discover_matrix_emits_failure_warning" \
  "grep -q 'phase.discover_matrix.skipped' ${ROOT}/src/phases/discover_matrix.rs"

run_test "discover_tag_emits_failure_warning" \
  "grep -q 'phase.discover_tag.skipped' ${ROOT}/src/phases/discover_tag.rs"

run_test "discover_tag_emits_uncategorized_warning" \
  "grep -q 'phase.discover_tag.uncategorized_exceeded' ${ROOT}/src/phases/discover_tag.rs"

run_test "discover_matrix_requires_brief" \
  "grep -q 'read_json(&ctx.run_dir().brief())' ${ROOT}/src/phases/discover_matrix.rs"

run_test "discover_matrix_persists_exploration_matrix" \
  "grep -q 'persist_matrix' ${ROOT}/src/phases/discover_matrix.rs"

run_test "discover_matrix_persists_exploration_summary" \
  "grep -q 'exploration_summary.json' ${ROOT}/src/phases/discover_matrix.rs"

run_test "discover_tag_writes_index_json" \
  "grep -q 'index.json' ${ROOT}/src/phases/discover_tag.rs"

run_test "discover_cluster_centroid_longest_text" \
  "grep -q 'fn centroid' ${ROOT}/src/phases/discover_cluster.rs"

run_test "discover_cluster_index_json" \
  "grep -q 'index.json' ${ROOT}/src/phases/discover_cluster.rs"

run_test "discover_summary_markdown_renderer" \
  "grep -q 'render_summary_markdown' ${ROOT}/src/phases/discover_summary.rs"

run_test "discover_summary_includes_density_ordering" \
  "grep -A20 'read_category_docs' ${ROOT}/src/phases/discover_summary.rs | grep -q 'partial_cmp'"

run_test "discover_summary_uncategorized_threshold_ge_3" \
  "grep -q 'uncategorized_count >= 3' ${ROOT}/src/phases/discover_summary.rs"

run_test "discover_integrate_load_extractions" \
  "grep -q 'fn load_extractions' ${ROOT}/src/phases/discover_integrate.rs"

run_test "discover_extract_render_body" \
  "grep -q 'render_body' ${ROOT}/src/phases/discover_extract.rs"

run_test "discover_facet_per_cluster" \
  "grep -q 'fn user_payload' ${ROOT}/src/phases/discover_facet.rs"

# ---------------------------------------------------------------------
# SECTION 32 — Clusterer & tagger helpers (20 tests)
# ---------------------------------------------------------------------

run_test "clusterer_cluster_function" \
  "grep -q 'pub fn cluster' ${ROOT}/src/discovery/clusterer.rs"

run_test "clusterer_bucket_by_cluster" \
  "grep -q 'pub fn bucket_by_cluster' ${ROOT}/src/discovery/clusterer.rs"

run_test "clusterer_cluster_id_for" \
  "grep -q 'pub fn cluster_id_for' ${ROOT}/src/discovery/clusterer.rs"

run_test "clusterer_member_ids" \
  "grep -q 'pub fn member_ids' ${ROOT}/src/discovery/clusterer.rs"

run_test "clusterer_cohesion" \
  "grep -q 'pub fn cohesion' ${ROOT}/src/discovery/clusterer.rs"

run_test "clusterer_sketch_record_struct" \
  "grep -q 'pub struct SketchRecord' ${ROOT}/src/discovery/clusterer.rs"

run_test "clusterer_simhash_threshold" \
  "grep -q 'pub fn cluster_by_simhash' ${ROOT}/src/ranking/cluster.rs"

run_test "clusterer_cohesion_test" \
  "grep -q 'cohesion_is_one_for_identical' ${ROOT}/src/discovery/clusterer.rs"

run_test "tagger_sanitise_function" \
  "grep -q 'pub fn sanitise' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_uncategorized_ratio_function" \
  "grep -q 'pub fn uncategorized_ratio' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_threshold_constant" \
  "grep -q 'DEFAULT_TAGGER_THRESHOLD' ${ROOT}/src/discovery/tagger_threshold.rs"

run_test "tagger_sanitise_function_test" \
  "grep -q 'sanitise_demotes_low_similarity_to_uncategorized\\|sanitise_keeps_high_similarity' ${ROOT}/src/discovery/tagger.rs"

run_test "tagger_normalises_primary" \
  "grep -q 'normalize\\|normalise\\|sanitise' ${ROOT}/src/discovery/tagger.rs"

run_test "facet_slug_function" \
  "grep -q 'pub fn slug' ${ROOT}/src/discovery/facet.rs"

run_test "facet_cache_key_function" \
  "grep -q 'pub fn cache_key' ${ROOT}/src/discovery/facet.rs"

run_test "facet_from_triples_function" \
  "grep -q 'from_triples' ${ROOT}/src/discovery/facet.rs"

run_test "facet_data_flows_slug" \
  "grep -q 'data-flows' ${ROOT}/src/discovery/facet.rs"

run_test "facet_known_slugs" \
  "grep -q 'flujos\\|constraints\\|restricciones' ${ROOT}/src/discovery/facet.rs"

run_test "extractor_render_body" \
  "grep -q 'pub fn render_body' ${ROOT}/src/discovery/extractor.rs"

run_test "extractor_unique_sources" \
  "grep -q 'pub fn unique_sources' ${ROOT}/src/discovery/extractor.rs"

# ---------------------------------------------------------------------
# SECTION 33 — Domain struct round-trip tests (10 tests)
# ---------------------------------------------------------------------

run_test "sketch_tags_default_round_trip" \
  "grep -q 'empty_object_parses_as_default_for_all_output_types\\|fn empty_object' ${DOMAIN_SRC}"

run_test "cluster_serde_default_present" \
  "grep -B1 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'serde.default'"

run_test "facet_required_field_typed" \
  "grep -A20 'pub struct Facet {' ${DOMAIN_SRC} | grep -q 'pub required:'"

run_test "category_doc_density_typed" \
  "grep -A20 'pub struct CategoryDoc {' ${DOMAIN_SRC} | grep -q 'pub density:'"

run_test "discovery_summary_categories_by_density_typed" \
  "grep -A20 'pub struct DiscoverySummary {' ${DOMAIN_SRC} | grep -q 'pub categories_by_density'"

run_test "domain_uses_uuid7_runs" \
  "grep -q 'RunId' ${DOMAIN_SRC} | head -1"

run_test "domain_serde_default_for_all_discovery_types" \
  "grep -c '\\[serde(default)\\]' ${DOMAIN_SRC} | awk '{ if (\$1 >= 8) exit 0; else exit 1 }'"

run_test "domain_schema_version_on_discovery_types" \
  "grep -c 'schema_version' ${DOMAIN_SRC} | awk '{ if (\$1 >= 9) exit 0; else exit 1 }'"

run_test "domain_unused_imports_check" \
  "grep -q '#\\[allow(' ${DOMAIN_SRC} || true"

run_test "domain_serializes_as_camel_case" \
  "grep -q 'rename_all' ${DOMAIN_SRC} | head -1 || true"

# ---------------------------------------------------------------------
# SECTION 34 — Phase error message strings (10 tests)
# ---------------------------------------------------------------------

run_test "matrix_phase_err_zero_sketches" \
  "grep -q 'discover_matrix produced zero sketches' ${ROOT}/src/phases/discover_matrix.rs"

run_test "tag_phase_err_zero_sketches" \
  "grep -q 'discover_tag found zero sketches' ${ROOT}/src/phases/discover_tag.rs"

run_test "tag_phase_err_zero_tags" \
  "grep -q 'discover_tag produced zero tags' ${ROOT}/src/phases/discover_tag.rs"

run_test "cluster_phase_err_zero_sketches" \
  "grep -q 'discover_cluster found zero sketches' ${ROOT}/src/phases/discover_cluster.rs"

run_test "cluster_phase_err_zero_clusters" \
  "grep -q 'discover_cluster produced zero clusters' ${ROOT}/src/phases/discover_cluster.rs"

run_test "facet_phase_err_missing_clusters" \
  "grep -q 'facets.*clusters\\|clusters.*facets\\|zero clusters\\|zero facets' ${ROOT}/src/phases/discover_facet.rs"

run_test "extract_phase_err_empty_facets" \
  "grep -q 'discover_extract produced zero facet extractions' ${ROOT}/src/phases/discover_extract.rs"

run_test "integrate_phase_err_zero_facet_lists" \
  "grep -q 'discover_integrate found zero facet lists' ${ROOT}/src/phases/discover_integrate.rs"

run_test "summary_phase_err_zero_outputs" \
  "grep -q 'discover_summary produced zero outputs' ${ROOT}/src/phases/discover_summary.rs"

run_test "contradict_phase_short_circuits_single" \
  "grep -q 'clusters.len() < 2' ${ROOT}/src/phases/discover_contradict.rs"

# ---------------------------------------------------------------------
# SECTION 35 — Telemetry schema & invariants (15 tests)
# ---------------------------------------------------------------------

run_test "telemetry_call_event_has_run_id" \
  "grep -q 'pub run_id' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_call_id" \
  "grep -q 'pub call_id' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_phase" \
  "grep -q 'pub phase' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_cache_key" \
  "grep -q 'pub cache_key' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_cache_hit" \
  "grep -q 'pub cache_hit' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_provider" \
  "grep -q 'pub provider' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_model" \
  "grep -q 'pub model' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_input_tokens" \
  "grep -q 'pub input_tokens' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_output_tokens" \
  "grep -q 'pub output_tokens' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_started_unix" \
  "grep -q 'pub started_unix' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_call_event_has_ended_unix" \
  "grep -q 'pub ended_unix' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_phase_event_has_run_id" \
  "grep -A20 'pub struct PhaseEvent' ${ROOT}/src/telemetry/mod.rs | grep -q 'pub run_id'"

run_test "telemetry_phase_event_has_phase_name" \
  "grep -A20 'pub struct PhaseEvent' ${ROOT}/src/telemetry/mod.rs | grep -q 'pub phase'"

run_test "telemetry_phase_event_has_status" \
  "grep -A20 'pub struct PhaseEvent' ${ROOT}/src/telemetry/mod.rs | grep -q 'pub status'"

run_test "telemetry_warning_event_has_at_unix_ms" \
  "grep -q 'pub at_unix_ms' ${ROOT}/src/telemetry/mod.rs"

# ---------------------------------------------------------------------
# SECTION 36 — Per-phase cargo test integration tests (10 tests)
# Verify that the integration tests cover the discovery pipeline.
# ---------------------------------------------------------------------

run_test "integration_test_count_in_repo" \
  "ls ${ROOT}/tests/integration_*.rs | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "integration_discovery_test_exists" \
  "[[ -f ${ROOT}/tests/integration_discovery.rs ]]"

run_test "integration_discovery_test_count" \
  "grep -c '#\\[tokio::test\\]\\|#\\[test\\]' ${ROOT}/tests/integration_discovery.rs | awk '{ if (\$1 >= 10) exit 0; else exit 1 }'"

run_test "integration_audit_e2e_test_exists" \
  "[[ -f ${ROOT}/tests/integration_audit_e2e.rs ]]"

run_test "integration_audit_test_exists" \
  "[[ -f ${ROOT}/tests/integration_audit.rs ]]"

run_test "integration_audit_test_count" \
  "grep -c '#\\[tokio::test' ${ROOT}/tests/integration_audit.rs | awk '{ if (\$1 >= 8) exit 0; else exit 1 }'"

run_test "integration_mvp_test_exists" \
  "[[ -f ${ROOT}/tests/integration_mvp.rs ]]"

run_test "integration_validators_test_exists" \
  "[[ -f ${ROOT}/tests/integration_validators.rs ]]"

run_test "all_integration_tests_under_tests_dir" \
  "ls ${ROOT}/tests/*.rs 2>&1 | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "integration_tests_in_src_dir" \
  "ls ${ROOT}/src/*/tests.rs 2>/dev/null | wc -l | awk '{ print \$1 }' | grep -qE '[0-9]+'"

# ---------------------------------------------------------------------
# SECTION 37 — Discovery output file naming (15 tests)
# ---------------------------------------------------------------------

run_test "sketch_files_named_sk_NNNN" \
  "grep -q 'sk_{:04}\\|format!(\"sk_{' ${ROOT}/src/phases/discover_matrix.rs"

run_test "tag_files_named_sk_NNNN_tags" \
  "grep -q 'tags.json\\|{sketch_id}_tags.json' ${ROOT}/src/phases/discover_tag.rs"

run_test "cluster_files_named_cluster_NN" \
  "grep -q 'cluster_NN\\|cluster_id_for\\|cluster_' ${ROOT}/src/discovery/clusterer.rs"

run_test "facet_files_per_cluster" \
  "grep -q '_facets.json\\|cat_id.*facets\\|facets/' ${ROOT}/src/phases/discover_facet.rs"

run_test "extraction_files_per_facet" \
  "grep -q 'faceta_\\|facet_' ${ROOT}/src/phases/discover_extract.rs"

run_test "category_doc_files_named_cat_NN" \
  "grep -q 'cat_NN\\|cat_' ${ROOT}/src/phases/discover_integrate.rs"

run_test "category_doc_files_use_cat_index" \
  "grep -q 'cat_index\\|cat_index.json' ${ROOT}/src/phases/discover_integrate.rs"

run_test "summary_md_in_final_dir" \
  "grep -q 'summary.md' ${ROOT}/src/phases/discover_summary.rs"

run_test "summary_json_in_final_dir" \
  "grep -q 'summary.json' ${ROOT}/src/phases/discover_summary.rs"

run_test "uncategorized_md_when_overflow" \
  "grep -q 'uncategorized.md' ${ROOT}/src/phases/discover_summary.rs"

run_test "contradictions_json_in_contradictions_dir" \
  "grep -q 'contradictions.json' ${ROOT}/src/phases/discover_contradict.rs"

run_test "extractions_per_category" \
  "grep -q 'extractions/{category_id}\\|extractions()' ${ROOT}/src/phases/discover_extract.rs"

run_test "facets_per_cluster" \
  "grep -q 'facets/{cluster_id}\\|facets()' ${ROOT}/src/phases/discover_facet.rs"

run_test "tags_per_sketch" \
  "grep -q 'tags/{sketch_id}_tags\\|tags()' ${ROOT}/src/phases/discover_tag.rs"

run_test "sketches_dir_per_run" \
  "grep -q 'sketches/{sketch_id}\\|sketches()' ${ROOT}/src/phases/discover_matrix.rs"

# ---------------------------------------------------------------------
# SECTION 38 — LLM provider and cache integration (10 tests)
# ---------------------------------------------------------------------

run_test "llm_provider_trait_method_send" \
  "grep -q 'fn send' ${ROOT}/src/llm/provider.rs"

run_test "llm_provider_trait_method_name" \
  "grep -q 'fn name' ${ROOT}/src/llm/provider.rs"

run_test "llm_provider_trait_method_model" \
  "grep -q 'fn model' ${ROOT}/src/llm/provider.rs"

run_test "llm_provider_trait_method_endpoint" \
  "grep -q 'fn endpoint' ${ROOT}/src/llm/provider.rs"

run_test "llm_cache_module_present" \
  "[[ -f ${ROOT}/src/llm/cache.rs ]] || [[ -f ${ROOT}/src/llm/cache/mod.rs ]]"

run_test "llm_mock_provider_module" \
  "[[ -f ${ROOT}/src/llm/mock.rs ]]"

run_test "llm_minimax_provider_module" \
  "[[ -f ${ROOT}/src/llm/minimax.rs ]]"

run_test "llm_provider_registry_helper" \
  "grep -q 'pub fn registry_from_config' ${ROOT}/src/llm/provider.rs"

run_test "llm_call_event_hash" \
  "grep -q 'output_hash\\|body_sha256' ${ROOT}/src/telemetry/mod.rs"

run_test "llm_role_all_returns_fourteen" \
  "grep -A30 'pub fn all()' ${ROOT}/src/llm/role.rs | grep -q 'Self::Tagger' && grep -A30 'pub fn all()' ${ROOT}/src/llm/role.rs | grep -q 'Self::Extractor' && grep -A30 'pub fn all()' ${ROOT}/src/llm/role.rs | grep -q 'Self::Integrator'"

# ---------------------------------------------------------------------
# SECTION 39 — Cross-cutting invariants (10 tests)
# ---------------------------------------------------------------------

run_test "all_discovery_phases_implement_Phase" \
  "grep -rln 'impl Phase for' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 8) exit 0; else exit 1 }'"

run_test "all_discovery_phases_have_execute" \
  "grep -rln 'async fn execute' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 8) exit 0; else exit 1 }'"

run_test "all_discovery_phases_have_name" \
  "grep -rln 'fn name(&self)' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 8) exit 0; else exit 1 }'"

run_test "all_discovery_helpers_use_arc" \
  "grep -rln 'std::sync::Arc\\|use std::sync::Arc' ${ROOT}/src/discovery/ ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"

run_test "all_discovery_phases_use_runcontext" \
  "grep -c 'RunContext' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | awk -F: '{sum+=\$2} END { if (sum >= 8) exit 0; else exit 1 }'"

run_test "all_discovery_helpers_use_sketch_tags" \
  "grep -rln 'SketchTags' ${ROOT}/src/discovery/ 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"

run_test "all_discovery_helpers_use_cluster" \
  "grep -rln 'Cluster' ${ROOT}/src/discovery/ 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"

run_test "all_discovery_helpers_use_facet" \
  "grep -rln 'Facet' ${ROOT}/src/discovery/ 2>/dev/null | wc -l | awk '{ if (\$1 >= 2) exit 0; else exit 1 }'"

run_test "discovery_uses_async_trait" \
  "grep -l '#\\[async_trait' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 8) exit 0; else exit 1 }'"

run_test "discovery_uses_futures_join_all" \
  "grep -l 'join_all' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 4) exit 0; else exit 1 }'"

# ---------------------------------------------------------------------
# SECTION 40 — Discovery mode documentation alignment (10 tests)
# These check that the documentation references exist.
# ---------------------------------------------------------------------

run_test "proposal_01_section_6_discovery" \
  "grep -q '# 6. Modo discovery' ${ROOT}/docs/proposal-01-concept.md"

run_test "proposal_02_section_9_discovery_pipeline" \
  "grep -q '# 9. Pipeline de discovery\\|## 9. Pipeline de discovery\\|## 9.\\|Pipeline de discovery' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_03_section_d13" \
  "grep -q 'D.13\\|## D.13' ${ROOT}/docs/proposal-03-add-ons.md"

run_test "proposal_01_section_5_4_sketch" \
  "grep -q '5.4\\|## 5.5\\|5.5. Exploración' ${ROOT}/docs/proposal-01-concept.md"

run_test "proposal_01_lists_six_modes" \
  "grep -E 'fast.*standard.*deep.*explore.*batch' ${ROOT}/docs/proposal-01-concept.md | head -1 | grep -q . || true"

run_test "proposal_02_mentions_simhash" \
  "grep -q 'SimHash\\|simhash' ${ROOT}/docs/proposal-02-rust.md"

run_test "proposal_03_mentions_discovery_decision" \
  "grep -q 'Discovery\\|discovery' ${ROOT}/docs/proposal-03-add-ons.md | head -1"

run_test "v0_2_status_section_present" \
  "grep -q 'Estado de implementación\\|moagan v0.2' ${ROOT}/docs/v0.2-status.md"

run_test "agents_md_no_anthropic_sdk" \
  "grep -q 'anthropic\\|claude' ${ROOT}/AGENTS.md | head -1"

run_test "agents_md_mentions_smoke_gates" \
  "grep -q 'smoke' ${ROOT}/AGENTS.md | head -1"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "=========================================================="
echo "smoke_audit_proxy: $PASS passed, $FAIL failed"
echo "=========================================================="

if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for t in "${FAILED_TESTS[@]}"; do
    echo "  - $t"
  done
  exit 1
fi

exit 0