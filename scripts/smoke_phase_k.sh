#!/usr/bin/env bash
# Smoke tests for sub-fase K (v0.4 add-ons):
#  - K.1 HARD_INCOMPATIBILITIES (constraint.rs + synthesize.rs)
#  - K.2 Embedder trait + HashingEmbedder (src/llm/embed)
#  - K.5 SQLite v008 migration + 5 new tables (storage/sqlite.rs)
#  - K.7 PatternKind enum + substitute + apply_with_categories
#  - K.9 retry_budget module (Mode × RetryReason matrix)
#
# The script focuses on the **public file surface** and a few
# cargo-driven assertions. The heavy unit / integration tests
# live in `src/llm/embed/mod.rs`, `src/storage/sqlite.rs`,
# `src/redact/{patterns,apply}.rs`, `src/llm/retry_budget.rs`
# and `tests/integration_phase_k.rs`.
#
# Each test sets the working directory to the repo root and
# asserts on either the filesystem layout (files exist, contain
# the expected grep matches) or the test runner output. The
# shell uses `set -uo pipefail` (no `-e`) so a single failing
# test does not abort the whole script; the final exit code is
# derived from the pass/fail counters.
#
# Usage:  ./scripts/smoke_phase_k.sh
# Exit:   0 when all tests pass, 1 otherwise.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PASS=0
FAIL=0
FAILED_TESTS=()

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

# ---------------------------------------------------------------------
# 1. K.1 — HARD_INCOMPATIBILITIES matrix + synthesize wiring
# ---------------------------------------------------------------------

run_test "k1_constraint_module_layout" '
  [[ -f '"$ROOT"'/src/domain/constraint.rs ]]
  [[ -f '"$ROOT"'/src/domain/mod.rs ]]
  grep -q "pub mod constraint;" '"$ROOT"'/src/domain/mod.rs
'

run_test "k1_hard_incompatibilities_constant" '
  grep -q "HARD_INCOMPATIBILITIES" '"$ROOT"'/src/domain/constraint.rs
  grep -q "(\"monolith\", \"microservices\")" '"$ROOT"'/src/domain/constraint.rs
  grep -q "(\"sql\", \"nosql\")" '"$ROOT"'/src/domain/constraint.rs
'

run_test "k1_is_incompatible_helper_present" '
  grep -q "pub fn is_incompatible" '"$ROOT"'/src/domain/constraint.rs
  grep -q "pub fn find_conflicts" '"$ROOT"'/src/domain/constraint.rs
'

run_test "k1_synthesize_phase_wires_constraint" '
  grep -q "HARD_INCOMPATIBILITIES" '"$ROOT"'/src/phases/synthesize.rs
  grep -q "cluster_conflict" '"$ROOT"'/src/phases/synthesize.rs
  grep -q "write_skipped_in_dir" '"$ROOT"'/src/phases/synthesize.rs
'

# ---------------------------------------------------------------------
# 2. K.2 — Embedder trait + HashingEmbedder
# ---------------------------------------------------------------------

run_test "k2_embed_module_layout" '
  [[ -f '"$ROOT"'/src/llm/embed/mod.rs ]]
'

run_test "k2_llm_mod_declares_embed" '
  grep -q "pub mod embed;" '"$ROOT"'/src/llm/mod.rs
'

run_test "k2_embedder_trait_and_hashing_impl" '
  grep -q "pub trait Embedder" '"$ROOT"'/src/llm/embed/mod.rs
  grep -q "HashingEmbedder" '"$ROOT"'/src/llm/embed/mod.rs
  grep -q "fn embed" '"$ROOT"'/src/llm/embed/mod.rs
  grep -q "fn dim" '"$ROOT"'/src/llm/embed/mod.rs
  grep -q "fn name" '"$ROOT"'/src/llm/embed/mod.rs
'

run_test "k2_fnv1a_helper_present" '
  grep -q "fn fnv1a_32" '"$ROOT"'/src/llm/embed/mod.rs
  grep -q "0x811c9dc5" '"$ROOT"'/src/llm/embed/mod.rs
  grep -q "0x01000193" '"$ROOT"'/src/llm/embed/mod.rs
'

run_test "k2_cosine_helper_present" '
  grep -q "pub fn cosine" '"$ROOT"'/src/llm/embed/mod.rs
'

run_test "k2_unit_tests_pass" '
  ( cd '"$ROOT"' && cargo test --lib embed:: >/tmp/smoke-k2 2>&1 )
  grep -q "test result: ok" /tmp/smoke-k2
'

# ---------------------------------------------------------------------
# 3. K.5 — SQLite v008 migration + 5 new tables
# ---------------------------------------------------------------------

run_test "k5_v008_migration_file_exists" '
  [[ -f '"$ROOT"'/src/storage/migrations/v008_add_ons.sql ]]
'

run_test "k5_v008_sql_defines_five_tables" '
  for t in outbox_events redact_audit manifest_events process_locks provider_rollups; do
    grep -q "CREATE TABLE IF NOT EXISTS $t" '"$ROOT"'/src/storage/migrations/v008_add_ons.sql \
      || { echo "missing CREATE TABLE for $t"; exit 1; }
  done
'

run_test "k5_sqlite_rs_registers_v008" '
  grep -q "sql_v008" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "v008_add_ons.sql" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "user_version = 8" '"$ROOT"'/src/storage/sqlite.rs
'

run_test "k5_sqlite_rs_exposes_v008_helpers" '
  grep -q "pub fn record_outbox_event" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn list_outbox_events_for_run" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn record_redact_audit" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn list_redact_audit_for_run" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn record_manifest_event" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn acquire_process_lock" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn release_process_lock" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn increment_provider_rollup" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn get_provider_rollup" '"$ROOT"'/src/storage/sqlite.rs
'

run_test "k5_sqlite_rs_exposes_row_types" '
  grep -q "pub struct OutboxEventRow" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub struct RedactAuditRow" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub struct ManifestEventRow" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub struct ProviderRollupRow" '"$ROOT"'/src/storage/sqlite.rs
'

run_test "k5_sqlite_v008_tests_pass" '
  ( cd '"$ROOT"' && cargo test --lib storage::sqlite::tests::v008 >/tmp/smoke-k5 2>&1 )
  grep -q "test result: ok" /tmp/smoke-k5
'

# ---------------------------------------------------------------------
# 4. K.7 — PatternKind + substitute + apply_with_categories
# ---------------------------------------------------------------------

run_test "k7_pattern_kind_enum_present" '
  grep -q "pub enum PatternKind" '"$ROOT"'/src/redact/patterns.rs
  grep -q "SkCpApiKey" '"$ROOT"'/src/redact/patterns.rs
  grep -q "BearerHeader" '"$ROOT"'/src/redact/patterns.rs
  grep -q "AnthropicApiKey" '"$ROOT"'/src/redact/patterns.rs
'

run_test "k7_substitute_helper_present" '
  grep -q "pub fn substitute" '"$ROOT"'/src/redact/patterns.rs
  grep -q "REDACTED:api_key:sk-cp" '"$ROOT"'/src/redact/patterns.rs
  grep -q "Bearer \*\*\*REDACTED\*\*\*" '"$ROOT"'/src/redact/patterns.rs
'

run_test "k7_apply_with_categories_present" '
  grep -q "apply_with_categories" '"$ROOT"'/src/redact/apply.rs
  grep -q "pub struct RedactResult" '"$ROOT"'/src/redact/apply.rs
'

run_test "k7_redact_tests_pass" '
  ( cd '"$ROOT"' && cargo test --lib redact:: >/tmp/smoke-k7 2>&1 )
  grep -q "test result: ok" /tmp/smoke-k7
'

# ---------------------------------------------------------------------
# 5. K.9 — Per-mode retry budget matrix
# ---------------------------------------------------------------------

run_test "k9_retry_budget_module_exists" '
  [[ -f '"$ROOT"'/src/llm/retry_budget.rs ]]
'

run_test "k9_llm_mod_declares_retry_budget" '
  grep -q "pub mod retry_budget;" '"$ROOT"'/src/llm/mod.rs
'

run_test "k9_budget_for_signature_present" '
  grep -q "pub fn budget_for" '"$ROOT"'/src/llm/retry_budget.rs
  grep -q "pub enum RetryReason" '"$ROOT"'/src/llm/retry_budget.rs
  grep -q "pub struct RetryBudget" '"$ROOT"'/src/llm/retry_budget.rs
'

run_test "k9_retry_budget_matrix_values" '
  grep -q "Mode::Deep, RetryReason::Parse" '"$ROOT"'/src/llm/retry_budget.rs
  grep -q "Mode::Deep, RetryReason::RateLimit" '"$ROOT"'/src/llm/retry_budget.rs
  grep -q "Mode::Standard, RetryReason::Parse" '"$ROOT"'/src/llm/retry_budget.rs
'

run_test "k9_retry_budget_tests_pass" '
  ( cd '"$ROOT"' && cargo test --lib retry_budget:: >/tmp/smoke-k9 2>&1 )
  grep -q "test result: ok" /tmp/smoke-k9
'

# ---------------------------------------------------------------------
# 6. Cross-cutting integration
# ---------------------------------------------------------------------

run_test "k_constraint_module_tests_pass" '
  ( cd '"$ROOT"' && cargo test --lib constraint:: >/tmp/smoke-k-cst 2>&1 )
  grep -q "test result: ok" /tmp/smoke-k-cst
'

run_test "k_integration_phase_k_tests_pass" '
  ( cd '"$ROOT"' && cargo test --test integration_phase_k >/tmp/smoke-k-int 2>&1 )
  grep -q "test result: ok" /tmp/smoke-k-int
'

run_test "k_clippy_clean" '
  ( cd '"$ROOT"' && cargo clippy --all-targets -- -D warnings >/tmp/smoke-k-clippy 2>&1 )
  rc=$?
  [[ $rc -eq 0 ]]
'

run_test "k_cargo_build_succeeds" '
  ( cd '"$ROOT"' && cargo build >/tmp/smoke-k-build 2>&1 )
  rc=$?
  [[ $rc -eq 0 ]]
'

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo
echo "Phase K smoke: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for name in "${FAILED_TESTS[@]}"; do
    echo "  - $name"
  done
  exit 1
fi
exit 0
