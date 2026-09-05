#!/usr/bin/env bash
# Smoke tests for Phase O (v0.3 sub-fase O): rubric anchoring
# (D.7.4) + compression enum / reader (D.7.5).
#
# Each test focuses on the **public surface** that downstream
# phases will import: the `Rubric` 1/3/5 anchors and the
# `Compression::reader` API. The heavy unit / integration tests
# live in src/ranking/rubric.rs, src/storage/compression.rs and
# tests/integration_phase_o.rs.
#
# Usage:  ./scripts/smoke_phase_o.sh
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

# ---------------------------------------------------------------------
# 1. Module presence
# ---------------------------------------------------------------------

run_test "ranking_module_lists_rubric" '
  grep -q "pub mod rubric;" '"$ROOT"'/src/ranking/mod.rs
'

run_test "rubric_file_present" '
  [[ -f '"$ROOT"'/src/ranking/rubric.rs ]]
'

run_test "compression_enum_present" '
  grep -q "pub enum Compression" '"$ROOT"'/src/storage/compression.rs
'

run_test "compression_reader_fn_present" '
  grep -q "pub fn reader" '"$ROOT"'/src/storage/compression.rs
'

# ---------------------------------------------------------------------
# 2. Re-exports
# ---------------------------------------------------------------------

run_test "ranking_mod_reexports_criterion" '
  grep -q "pub use rubric::{Criterion, Rubric}" '"$ROOT"'/src/ranking/mod.rs
'

# ---------------------------------------------------------------------
# 3. Unit tests pin the seeded anchors
# ---------------------------------------------------------------------

run_test "rubric_unit_tests_pass" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --lib ranking::rubric::tests --quiet) >/dev/null
'

run_test "compression_unit_tests_pass" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --lib storage::compression::tests --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 4. Integration tests (round-trip gz, zst, none)
# ---------------------------------------------------------------------

run_test "integration_phase_o_passes" '
  (cd '"$ROOT"' && MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_o --quiet) >/dev/null
'

# ---------------------------------------------------------------------
# 5. Anchors cover every (criterion, level) cell
# ---------------------------------------------------------------------

run_test "rubric_seeds_18_anchors" '
  # 6 criteria x 3 levels = 18 anchored strings.
  count=$(grep -c "m.insert" '"$ROOT"'/src/ranking/rubric.rs || true)
  [[ "$count" -ge 18 ]]
'

run_test "rubric_anchored_1_method_present" '
  grep -q "pub fn anchored_1" '"$ROOT"'/src/ranking/rubric.rs
'

run_test "rubric_anchored_3_method_present" '
  grep -q "pub fn anchored_3" '"$ROOT"'/src/ranking/rubric.rs
'

run_test "rubric_anchored_5_method_present" '
  grep -q "pub fn anchored_5" '"$ROOT"'/src/ranking/rubric.rs
'

# ---------------------------------------------------------------------
# 6. Compression enum covers all three modes
# ---------------------------------------------------------------------

run_test "compression_variants_three" '
  count=$(grep -E "^\s+(None|Gz|Zst)," '"$ROOT"'/src/storage/compression.rs | wc -l)
  [[ "$count" -ge 3 ]]
'

run_test "compression_from_extension_method_present" '
  grep -q "pub fn from_extension" '"$ROOT"'/src/storage/compression.rs
'

# ---------------------------------------------------------------------
# 7. CI guards still pass
# ---------------------------------------------------------------------

run_test "no_anthropic_sdk_in_source" '
  bash '"$ROOT"'/scripts/check-no-anthropic-sdk.sh
'

run_test "forbidden_crates_check_passes" '
  bash '"$ROOT"'/scripts/check-no-forbidden-crates.sh
'

# ---------------------------------------------------------------------
# 8. Docs annotations
# ---------------------------------------------------------------------

run_test "proposal_03_annotates_d74_d75" '
  grep -q "D.7.4" '"$ROOT"'/docs/proposal-03-add-ons.md
  grep -q "D.7.5" '"$ROOT"'/docs/proposal-03-add-ons.md
'

run_test "v0_3_status_mentions_subfase_o" '
  grep -qi "sub.fase O\|phase O" '"$ROOT"'/docs/v0.3-status.md
'

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo
echo "Phase O smoke: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  for t in "${FAILED_TESTS[@]}"; do
    echo "  - $t"
  done
  exit 1
fi
exit 0
