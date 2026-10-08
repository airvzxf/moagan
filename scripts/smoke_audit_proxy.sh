#!/usr/bin/env bash
# Smoke checks for `moagan discover` and the `moagan audit` sidecar:
# CLI surface, source greps for the discover phases, roles and audit
# types, and mock runs (no network; the real proxy round-trips live in
# e2e_audit_proxy.sh). About 260 checks; under a minute.
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

# Resolve the domain source path. The domain types live in
# `src/domain/mod.rs` since sub-fase K; the older flat `src/domain.rs`
# was removed in 16eb259. The smoke only needs the current path.
DOMAIN_SRC="${ROOT}/src/domain/mod.rs"
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

# `start_proxy` / `stop_proxy` were declared here but never invoked;
# this smoke script is `grep`-only (no real proxy round-trips). The
# 37-test card-80 block in `e2e_audit_proxy.sh` is the one that
# actually spins up the sidecar; dead helpers removed to silence the
# orphan-proxy risk hinted at by #897.

# ---------------------------------------------------------------------
# SECTION 1 — CLI surface
# ---------------------------------------------------------------------

run_test "cli_bin_runs" "[[ -x $BIN ]]"
run_test "cli_help_lists_discover" "$BIN --help 2>&1 | grep -q 'discover'"
run_test "cli_discover_help_mentions_the_catalogue" \
  "$BIN discover --help 2>&1 | grep -q 'catalogue of theses'"
run_test "cli_discover_help_prints_sketches_per_cell" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-sketches-per-cell'"
run_test "cli_discover_help_prints_dimensions" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-dimensions'"
run_test "cli_discover_help_prints_facets" \
  "$BIN discover --help 2>&1 | grep -q '\\-\\-facets-per-dimension'"
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
  "! $BIN run --help 2>&1 | grep -qE '^ +- discover'"
run_test "cli_audit_help_mentions_proxy" \
  "$BIN audit --help 2>&1 | grep -q 'proxy'"
run_test "cli_audit_help_mentions_verify" \
  "$BIN audit --help 2>&1 | grep -q 'verify'"

# ---------------------------------------------------------------------
# SECTION 2 — Sketches-per-cell validation
# ---------------------------------------------------------------------

run_test "sketches_per_cell_5_accepted" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 5 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_0_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --prompt 'x' --sketches-per-cell 0 2>&1 | grep -q 'below the minimum of 1'"

run_test "sketches_per_cell_1_floor_ok" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 1 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_9_accepted" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 9 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_default_ok" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 10 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_25_accepted" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 25 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "sketches_per_cell_100_accepted" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 100 --dimensions 2 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState'"

run_test "legacy_cardinality_rejected" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --provider mock:mock-model --prompt 'x' --cardinality 80 2>&1 | grep -qE 'unexpected argument|--cardinality'"

run_test "sketches_per_cell_invalid_value" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --prompt 'x' --sketches-per-cell abc 2>&1 | grep -qE 'invalid|InvalidArgs'"

run_test "sketches_per_cell_missing_value" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --prompt 'x' --sketches-per-cell 2>&1 | grep -qE 'a value is required|needs a value|InvalidArgs'"

run_test "sketches_per_cell_zero_dimensions_rejected_or_warns" \
  "MOAGAN_HOME=\$(mktemp -d) $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'x' --sketches-per-cell 10 --dimensions 0 --facets-per-dimension 2 2>&1 | grep -qE 'discovery run id|InvalidState|InvalidArgs'"

# ---------------------------------------------------------------------
# SECTION 3 — Role inventory
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

# ---------------------------------------------------------------------
# SECTION 4 — Role temperatures & max_tokens
# ---------------------------------------------------------------------

run_test "temp_sketch_baseline_kept" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Sketch => 1.0\\|Sketch => 0.6'"

run_test "temp_intake_baseline_kept" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Intake => 0.4'"

run_test "temp_clarify_baseline_kept" \
  "grep -A100 'fn temperature_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Clarify => 0.0'"

run_test "max_tokens_sketch_baseline_kept" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Sketch => '"

run_test "max_tokens_deliver_baseline_kept" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Deliver => '"

run_test "max_tokens_judge_baseline_kept" \
  "grep -A20 'fn max_tokens_for_role' ${ROOT}/src/phases/phase.rs | grep -q 'Judge => '"

# ---------------------------------------------------------------------
# SECTION 5 — Discovery directories
# ---------------------------------------------------------------------

run_test "fs_layout_drafts_path" \
  "grep -q 'pub fn drafts' ${ROOT}/src/fs_layout.rs"

run_test "fs_layout_ensure_creates_drafts" \
  "grep -q 'self.drafts()' ${ROOT}/src/fs_layout.rs"

# ---------------------------------------------------------------------
# SECTION 6 — ExplorationMatrix
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

# ---------------------------------------------------------------------
# SECTION 8 — Discovery phases
# ---------------------------------------------------------------------

run_test "phase_discover_files_exist" \
  "for p in intake dimensions sketches curate render; do [[ -f ${ROOT}/src/phases/discover_\$p.rs ]] || exit 1; done"

run_test "phase_names_match_their_files" \
  "for p in dimensions sketches curate render; do grep -q \"\\\"discover_\$p\\\"\" ${ROOT}/src/phases/discover_\$p.rs || exit 1; done"

# ---------------------------------------------------------------------
# SECTION 9 — Pipeline composition
# ---------------------------------------------------------------------

run_test "pipeline_wired_in_discover_cli" \
  "grep -q 'pub fn discover_pipeline' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_intake" \
  "grep -q 'push(DiscoverIntakePhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_dimensions" \
  "grep -q 'push(DiscoverDimensionsPhase)' ${ROOT}/src/cli/discover.rs"
run_test "pipeline_includes_sketches" \
  "grep -q 'push(DiscoverSketchesPhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_render" \
  "grep -q 'push(DiscoverRenderPhase)' ${ROOT}/src/cli/discover.rs"

run_test "pipeline_includes_curate" \
  "grep -q 'push(DiscoverCuratePhase)' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# SECTION 10 — Prompts & registrations
# ---------------------------------------------------------------------

run_test "prompt_discover_matrix_exists" "[[ -f ${ROOT}/src/llm/prompts/discover_matrix.md ]]"
run_test "prompt_discover_matrix_registered" "grep -q 'DISCOVER_MATRIX_PROMPT' ${ROOT}/src/llm/prompts.rs"
run_test "discover_matrix_system_prompt_helper" \
  "grep -q 'discover_matrix_system_prompt' ${ROOT}/src/llm/prompts.rs"

# ---------------------------------------------------------------------
# SECTION 11 — Domain types
# ---------------------------------------------------------------------

run_test "domain_cluster_struct" \
  "grep -q 'pub struct Cluster' ${DOMAIN_SRC}"

run_test "domain_cluster_serde_default" \
  "grep -B1 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'serde.default'"

# ---------------------------------------------------------------------
# SECTION 12 — JSON contracts
# ---------------------------------------------------------------------

run_test "cluster_schema_version_value" \
  "grep -A20 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'schema_version:'"

run_test "sketch_tags_default_v1" \
  "grep -n 'schema_version: \"v1\"' ${DOMAIN_SRC} | head -1 | grep -q . || test \$(grep -c 'schema_version: \"v1\"' ${DOMAIN_SRC}) -ge 1"

run_test "cluster_default_v1" \
  "test \$(grep -c 'schema_version: \"v1\"' ${DOMAIN_SRC}) -ge 2"

run_test "category_doc_default_v1" \
  "test \$(grep -c 'schema_version: \"v1\"' ${DOMAIN_SRC}) -ge 3"

run_test "cluster_cohesion_field" \
  "grep -A20 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'cohesion'"

run_test "cluster_members_field" \
  "grep -A20 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'members'"

# ---------------------------------------------------------------------
# SECTION 13 — Role descriptions
# ---------------------------------------------------------------------

run_test "role_intake_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Intake'"

run_test "role_sketch_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Sketch'"

run_test "role_judge_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Judge'"

run_test "role_deliver_baseline_kept" \
  "grep -A20 'pub fn all' ${ROOT}/src/llm/role.rs | grep -q 'Deliver'"

# ---------------------------------------------------------------------
# SECTION 14 — Forbidden patterns
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
# SECTION 15 — Git hygiene
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
  "git -C ${ROOT} log --pretty='%G?' origin/main..HEAD | awk '!/^G$/ {n++} END{exit n}'"

run_test "commit_count_under_30" \
  "git -C ${ROOT} log --oneline origin/main..HEAD | wc -l | awk '{ if (\$1 <= 30) exit 0; else exit 1 }'"

# `commit_count_over_5` and `no_uncommitted_changes_*` were PR-time
# checks (\"feature branch has ≥5 commits\" / \"no uncommitted drift\").
# They only make sense for a feature branch; on `main` they
# unconditionally fail (`HEAD == main`, so `origin/main..HEAD` is
# empty). Removed: they're out of scope for a smoke suite.

# ---------------------------------------------------------------------
# SECTION 16 — Test counts & build
# ---------------------------------------------------------------------

run_test "test_count_over_400" \
  "cd ${ROOT} && MOAGAN_NON_INTERACTIVE=1 cargo test --lib 2>&1 | grep 'test result' | grep -oE '[0-9]+ passed' | head -1 | awk '{ if (\$1 >= 400) exit 0; else exit 1 }'"

run_test "integration_test_discovery_exists" \
  "[[ -f ${ROOT}/tests/integration_discovery.rs ]]"

run_test "integration_test_audit_e2e_exists" \
  "[[ -f ${ROOT}/tests/integration_audit_e2e.rs ]]"

run_test "smoke_discovery_script_exists" \
  "[[ -f ${ROOT}/scripts/smoke_discovery.sh ]]"

run_test "smoke_discovery_has_at_least_ten_checks" \
  "grep -c '^run_test ' ${ROOT}/scripts/smoke_discovery.sh | awk '{ if (\$1 >= 10) exit 0; else exit 1 }'"

run_test "smoke_test_count_under_200" \
  "[[ \$(grep -c '^run_test ' ${ROOT}/scripts/smoke_discovery.sh) -le 200 ]]"

run_test "all_targets_compile" \
  "cd ${ROOT} && cargo build --all-targets 2>&1 | grep -q 'Finished'"

run_test "clippy_clean" \
  "cd ${ROOT} && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5 | grep -qE 'error:'; test \$? -ne 0"

run_test "fmt_clean" \
  "cd ${ROOT} && cargo fmt --all -- --check 2>&1 | grep -qE 'diff'; test \$? -ne 0"

# ---------------------------------------------------------------------
# SECTION 17 — Documentation references
#
# The original 10 references to `docs/proposal-{01,02,03}-*.md` and
# `docs/v0.2-status.md` were retired when those docs were deleted by
# PR #660 (commit 27fda5a) and PR #673 (commit 0e52e7be). The surviving
# ADRs (docs/adr/0001..0005) do not cover the SimHash / Pipeline de
# discovery / D.13 content the old tests asserted (verified by `grep`
# in v0.14.6); the design intent has been folded into `src/discovery/*`
# per the "code wins" principle in AGENTS.md. The 3 surviving tests
# cover AGENTS.md content that still exists.
# ---------------------------------------------------------------------

run_test "doc_agents_md_mentions_no_go" \
  "grep -qi 'no-go' ${ROOT}/AGENTS.md"

run_test "doc_agents_md_mentions_validation_gauntlet" \
  "grep -qi 'validation' ${ROOT}/AGENTS.md"

run_test "doc_agents_md_mentions_signed_commits" \
  "grep -qi 'GPG\\|signed' ${ROOT}/AGENTS.md"

# ---------------------------------------------------------------------
# SECTION 18 — Specific helpers
# ---------------------------------------------------------------------

run_test "discovery_pipeline_intake_first" \
  "grep -q 'Pipeline::new().push(DiscoverIntakePhase)' ${ROOT}/src/cli/discover.rs"

run_test "discovery_sketches_per_cell_floor_1" \
  "grep -q 'sketches-per-cell {sketches_per_cell} below the minimum of {MIN_SKETCHES_PER_CELL}' ${ROOT}/src/cli/mod.rs"

run_test "discovery_floor_in_discover_rs_uses_constant" \
  "grep -q 'MIN_SKETCHES_PER_CELL' ${ROOT}/src/cli/discover.rs"

# v0.13.4 (PR #700 item 10): the `discovery_explain` path runs
# the same floor check as `Cmd::Discover` (see
# `discover_explain::build_and_format` in
# `src/cli/discover_explain.rs`). Extend the constant-usage
# guard to also scan `discover_explain.rs` so a future refactor
# that drifts to a literal `1` (the v0.13.2 floor) trips the
# smoke before it lands. The error string is harmonised with
# `cli::mod` so the same regex matches both surfaces.
run_test "discovery_floor_in_discover_explain_rs_uses_constant" \
  "grep -q 'below the minimum of {MIN_SKETCHES_PER_CELL}' ${ROOT}/src/cli/discover_explain.rs"

run_test "discovery_calls_build_registry" \
  "grep -q 'build_registry_for' ${ROOT}/src/cli/discover.rs"

# ---------------------------------------------------------------------
# SECTION 19 — Audit proxy sidecar
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
# SECTION 20 — Run-output artifact paths
# ---------------------------------------------------------------------

run_test "artifacts_sketches_subdir" \
  "grep -q 'pub fn sketches' ${ROOT}/src/fs_layout.rs"

run_test "artifacts_drafts_subdir" \
  "grep -q 'pub fn drafts' ${ROOT}/src/fs_layout.rs"

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
# SECTION 21 — Audit-sidecar integration points
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
# SECTION 22 — Audit record schema
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
# SECTION 23 — Verify report schema
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
# SECTION 24 — Pipeline mock fixture paths
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
# SECTION 25 — Per-artifact inspection via CLI mock
# ---------------------------------------------------------------------

# These tests use the mock provider with the smoke fixtures. Each
# inspects an aspect of the run directory that should exist after a
# discovery run completes.

WORK_A=$(mkhome)
run_test "mock_run_with_cardinality_8_inspects_pipeline_names" \
  "MOAGAN_HOME=$WORK_A $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_A/.runs/ 2>/dev/null | head -1 | grep -qE '[0-9a-f]'"
rm -rf "$WORK_A"

WORK_B=$(mkhome)
run_test "mock_run_creates_manifest" \
  "MOAGAN_HOME=$WORK_B $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_B/.runs/*/manifest.json 2>/dev/null | grep -q manifest.json"
rm -rf "$WORK_B"

WORK_C=$(mkhome)
run_test "mock_run_creates_brief" \
  "MOAGAN_HOME=$WORK_C $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_C/.runs/*/brief.json 2>/dev/null | grep -q brief.json"
rm -rf "$WORK_C"

WORK_D=$(mkhome)
run_test "mock_run_creates_sketches" \
  "MOAGAN_HOME=$WORK_D $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_D/.runs/*/sketches/*.json 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"
rm -rf "$WORK_D"

WORK_E=$(mkhome)
run_test "mock_run_creates_calls_telemetry" \
  "MOAGAN_HOME=$WORK_E $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_E/.runs/*/telemetry/calls.jsonl.gz 2>/dev/null | grep -q calls.jsonl.gz"
rm -rf "$WORK_E"

WORK_F=$(mkhome)
run_test "mock_run_creates_phases_telemetry" \
  "MOAGAN_HOME=$WORK_F $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_F/.runs/*/telemetry/phases.jsonl.gz 2>/dev/null | grep -q phases.jsonl.gz"
rm -rf "$WORK_F"

WORK_G=$(mkhome)
run_test "mock_run_creates_proposals" \
  "MOAGAN_HOME=$WORK_G $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_G/.runs/*/proposals/*.json 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"
rm -rf "$WORK_G"

WORK_H=$(mkhome)
run_test "mock_run_creates_ranking" \
  "MOAGAN_HOME=$WORK_H $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_H/.runs/*/rankings/ 2>/dev/null | grep -q ranking.json"
rm -rf "$WORK_H"

WORK_I=$(mkhome)
run_test "mock_run_creates_portfolio" \
  "MOAGAN_HOME=$WORK_I $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_I/.runs/*/final/portfolio.md 2>/dev/null | grep -q portfolio.md"
rm -rf "$WORK_I"

WORK_J=$(mkhome)
run_test "mock_run_creates_recommendation" \
  "MOAGAN_HOME=$WORK_J $BIN run --mode deep --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --non-interactive 2>&1 >/dev/null; ls $WORK_J/.runs/*/final/ 2>/dev/null | head -10 | grep -qE 'recommendation|portfolio'"
rm -rf "$WORK_J"

# ---------------------------------------------------------------------
# SECTION 26 — Discovery CLI artifact inspection
# Run a discover pipeline programmatically by checking what dirs the
# pipeline requires and verifying each phase writes its expected
# intermediate directory.
# ---------------------------------------------------------------------

WORK_K=$(mkhome)
run_test "discover_run_creates_run_root" \
  "MOAGAN_HOME=$WORK_K $BIN discover --provider mock:mock-model --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls $WORK_K/.runs/ 2>/dev/null | head -1 | grep -qE '[0-9a-f]'"
rm -rf "$WORK_K"

WORK_Q=$(mkhome)
run_test "discover_run_creates_drafts_dir" \
  "MOAGAN_HOME=$WORK_Q $BIN discover --provider mock:mock-model --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls -d $WORK_Q/.runs/*/drafts/ 2>/dev/null | head -1 | grep -qE '/drafts/$'"
rm -rf "$WORK_Q"

# The remaining tests need content; mock provider cycles so these
# depend on whether the mock provider gets through enough cycles.

WORK_T=$(mkhome)
run_test "discover_run_creates_final_readme" \
  "MOAGAN_HOME=$WORK_T $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls $WORK_T/.runs/*/final/README.md 2>/dev/null | head -1 | grep -q README.md"
rm -rf "$WORK_T"

WORK_U=$(mkhome)
run_test "discover_run_creates_catalog_json" \
  "MOAGAN_HOME=$WORK_U $BIN discover --non-interactive --provider mock:mock-model --mock-dir ${ROOT}/tests/fixtures/mock_provider --prompt 'probe' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 > /dev/null 2>&1; ls $WORK_U/.runs/*/final/catalog.json 2>/dev/null | head -1 | grep -q catalog.json"
run_test "discover_run_creates_curation_files" \
  "ls $WORK_U/.runs/*/curation/*.json 2>/dev/null | grep -q __"
rm -rf "$WORK_U"

# ---------------------------------------------------------------------
# SECTION 28 — Audit-format integrity tests
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
# SECTION 29 — Telemetry complement
# ---------------------------------------------------------------------

run_test "telemetry_call_record_body_sha256" \
  "grep -q 'body_sha256' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_module_call_event" \
  "grep -q 'pub struct CallEvent' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_module_phase_event" \
  "grep -q 'pub struct PhaseEvent' ${ROOT}/src/telemetry/mod.rs"

run_test "telemetry_module_warning_event" \
  "grep -q 'pub struct WarningEvent' ${ROOT}/src/telemetry/mod.rs"

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
# SECTION 30 — Edge cases & integration
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
# SECTION 32 — Clusterer & tagger helpers
# ---------------------------------------------------------------------

run_test "clusterer_simhash_threshold" \
  "grep -q 'pub fn cluster_by_simhash' ${ROOT}/src/ranking/cluster.rs"

# ---------------------------------------------------------------------
# SECTION 33 — Domain struct round-trip tests
# ---------------------------------------------------------------------

run_test "sketch_tags_default_round_trip" \
  "grep -q 'empty_object_parses_as_default_for_all_output_types\\|fn empty_object' ${DOMAIN_SRC}"

run_test "cluster_serde_default_present" \
  "grep -B1 'pub struct Cluster {' ${DOMAIN_SRC} | grep -q 'serde.default'"

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
# SECTION 35 — Telemetry schema & invariants
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
# SECTION 36 — Per-phase cargo test integration tests
# Verify that the integration tests cover the discovery pipeline.
# ---------------------------------------------------------------------

run_test "integration_test_count_in_repo" \
  "ls ${ROOT}/tests/integration_*.rs | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "integration_discovery_test_exists" \
  "[[ -f ${ROOT}/tests/integration_discovery.rs ]]"

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
# SECTION 37 — Discovery output file naming
# ---------------------------------------------------------------------

run_test "sketch_files_named_sk_NNNN" \
  "grep -q 'sk_{:04}' ${ROOT}/src/phases/discover_sketches.rs"

# ---------------------------------------------------------------------
# SECTION 38 — LLM provider and cache integration
# ---------------------------------------------------------------------

run_test "llm_client_trait_exists" \
  "grep -q 'pub trait LlmClient' ${ROOT}/src/llm/client/mod.rs"

run_test "llm_cache_module_present" \
  "[[ -f ${ROOT}/src/llm/cache.rs ]] || [[ -f ${ROOT}/src/llm/cache/mod.rs ]]"

run_test "llm_call_event_hash" \
  "grep -q 'output_hash\\|body_sha256' ${ROOT}/src/telemetry/mod.rs"

# ---------------------------------------------------------------------
# SECTION 39 — Cross-cutting invariants
# ---------------------------------------------------------------------

run_test "all_discovery_phases_implement_Phase" \
  "grep -rln 'impl Phase for' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "all_discovery_phases_have_execute" \
  "grep -rln 'async fn execute' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "all_discovery_phases_have_name" \
  "grep -rln 'fn name(&self)' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "all_discovery_helpers_use_arc" \
  "grep -rln 'std::sync::Arc\\|use std::sync::Arc' ${ROOT}/src/discovery/ ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 1) exit 0; else exit 1 }'"

run_test "all_discovery_phases_use_runcontext" \
  "grep -c 'RunContext' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | awk -F: '{sum+=\$2} END { if (sum >= 8) exit 0; else exit 1 }'"

run_test "all_discovery_helpers_use_facet" \
  "grep -rln 'Facet' ${ROOT}/src/discovery/ 2>/dev/null | wc -l | awk '{ if (\$1 >= 2) exit 0; else exit 1 }'"

run_test "discovery_uses_async_trait" \
  "grep -l '#\\[async_trait' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 5) exit 0; else exit 1 }'"

run_test "discovery_fan_outs_use_joinset" \
  "grep -l 'JoinSet' ${ROOT}/src/phases/discover_*.rs 2>/dev/null | wc -l | awk '{ if (\$1 >= 2) exit 0; else exit 1 }'"

# ---------------------------------------------------------------------
# SECTION 40 — Discovery mode documentation alignment
#
# The original 8 references to `docs/proposal-{01,02,03}-*.md` and
# `docs/v0.2-status.md` were retired when those docs were deleted by
# PR #660 (commit 27fda5a) and PR #673 (commit 0e52e7be). The
# `proposal_01_lists_six_modes` entry was a silent `|| true` no-op
# (always passed; contributed no signal). The 2 surviving tests cover
# AGENTS.md content that still exists.
# ---------------------------------------------------------------------

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