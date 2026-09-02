#!/usr/bin/env bash
# Smoke tests for the `moagan preflight` subcommand (PR #566).
#
# Preflight runs the FULL pipeline end-to-end against the real
# provider in two steps:
# 1. `moagan discover` with cardinalidad 8 + 1 temp + 1 replica
# 2. `moagan run --mode fast` with `--context <discover_run_id>`
#    so the second run consumes the discover run's library
#
# The smoke verifies both runs land in the expected dirs and both
# run ids are printed. Mock provider is used so the test is
# deterministic and does not require MINIMAX_API_KEY in CI.
#
# Usage:  ./scripts/smoke_preflight.sh
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
  bash -c "$body" >/tmp/smoke-preflight-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-preflight-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

# ---------------------------------------------------------------------
# Test 1: preflight with mock provider returns both run ids
# ---------------------------------------------------------------------

run_test "preflight_two_run_ids_printed" "
  set -euo pipefail
  HOME_DIR=\$(mktemp -d /tmp/moagan-preflight.XXXXXX)
  export MOAGAN_HOME=\$HOME_DIR
  export MOAGAN_QUIET=1
  output=\$($BIN preflight \
    --provider mock:mock-model \
    --prompt 'demo prompt' \
    --runs-dir \$HOME_DIR/.runs \
    --mock-dir tests/fixtures/mock_provider \
    --max-parallelism 4 \
    --non-interactive 2>&1) || {
    echo 'preflight binary failed'
    echo \"\$output\"
    exit 1
  }
  # Write the output to a file so we can inspect on failure
  echo \"\$output\" > /tmp/preflight-out.txt
  echo \"preflight output len: \${#output}\"
  echo '--- searching for run ids ---'
  grep -E 'preflight (discover|fast) run_id' /tmp/preflight-out.txt | head -3
  echo '---'
  echo \"\$output\" | grep -cE 'preflight discover run_id: ' | grep -qE '^([1-9][0-9]*)$' || { echo 'no discover run_id line'; exit 1; }
  echo \"\$output\" | grep -cE 'preflight fast run_id: '     | grep -qE '^([1-9][0-9]*)$' || { echo 'no fast run_id line'; exit 1; }
"

# ---------------------------------------------------------------------
# Test 2: preflight mock provider produces a discover run dir with
# the sketches library (smoke-level: just verify the dir exists
# and has at least one sketch file)
# ---------------------------------------------------------------------

run_test "preflight_mock_creates_discover_sketches" "
  set -euo pipefail
  HOME_DIR=\$(mktemp -d /tmp/moagan-preflight.XXXXXX)
  export MOAGAN_HOME=\$HOME_DIR
  export MOAGAN_QUIET=1
  $BIN preflight \
    --provider mock:mock-model \
    --prompt 'demo prompt' \
    --runs-dir \$HOME_DIR/.runs \
    --mock-dir tests/fixtures/mock_provider \
    --max-parallelism 4 \
    --non-interactive > /tmp/preflight.out 2>&1 || {
    echo 'preflight binary failed'
    cat /tmp/preflight.out
    exit 1
  }
  discover_id=\$(grep -E 'preflight discover run_id: ' /tmp/preflight.out | awk '{print \$NF}')
  if [[ -z \"\$discover_id\" ]]; then echo 'no discover run id'; exit 1; fi
  discover_dir=\$HOME_DIR/.runs/\$discover_id
  test -d \"\$discover_dir\" || { echo \"discover dir missing: \$discover_dir\"; exit 1; }
  test -d \"\$discover_dir/sketches\" || { echo 'sketches dir missing'; exit 1; }
  # Cardinalidad 8 = 8 sketches on disk
  sketch_count=\$(find \"\$discover_dir/sketches\" -name 'sk_*.json' -not -name '*.meta.json' | wc -l)
  if [[ \"\$sketch_count\" -lt 1 ]]; then
    echo \"expected >= 1 sketch, got \$sketch_count\"
    exit 1
  fi
"

# ---------------------------------------------------------------------
# Test 3: preflight handles --non-interactive correctly (no prompts)
# ---------------------------------------------------------------------

run_test "preflight_non_interactive_no_prompts" "
  set -euo pipefail
  HOME_DIR=\$(mktemp -d /tmp/moagan-preflight.XXXXXX)
  export MOAGAN_HOME=\$HOME_DIR
  export MOAGAN_QUIET=1
  echo n | $BIN preflight \
    --provider mock:mock-model \
    --prompt 'demo prompt' \
    --runs-dir \$HOME_DIR/.runs \
    --mock-dir tests/fixtures/mock_provider \
    --max-parallelism 4 \
    --non-interactive > /tmp/preflight.out 2>&1 || {
    echo 'preflight failed'
    cat /tmp/preflight.out
    exit 1
  }
  ! grep -q 'Press enter' /tmp/preflight.out || { echo 'interactive prompt leaked'; exit 1; }
"

# ---------------------------------------------------------------------
# Test 4: preflight rejects bad provider (sanity gate)
# ---------------------------------------------------------------------

run_test "preflight_invalid_provider_fails_fast" "
  set -euo pipefail
  HOME_DIR=\$(mktemp -d /tmp/moagan-preflight.XXXXXX)
  export MOAGAN_HOME=\$HOME_DIR
  export MOAGAN_QUIET=1
  # mixed-up provider = no valid provider in config; the binary
  # fails on lookup, not on the LLM call. This is a sanity check
  # that the preflight does not silently succeed with a typo.
  if $BIN preflight \
    --provider 'no-such-provider-xyz' \
    --prompt 'demo' \
    --runs-dir \$HOME_DIR/.runs \
    --mock-dir tests/fixtures/mock_provider \
    --max-parallelism 2 \
    --non-interactive > /tmp/preflight.out 2>&1; then
    echo 'expected preflight to fail on unknown provider'
    exit 1
  fi
  grep -qE 'provider' /tmp/preflight.out || { echo 'no provider error'; exit 1; }
"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "preflight smoke: PASS=$PASS FAIL=$FAIL"
if [[ $FAIL -gt 0 ]]; then
  echo "FAILED: ${FAILED_TESTS[*]}"
  exit 1
fi
echo "OK: preflight smoke passed"
