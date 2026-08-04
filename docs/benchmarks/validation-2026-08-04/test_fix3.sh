#!/usr/bin/env bash
# Verify --switch-provider rejects unknown providers
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-runs/fix3-$$"
mkdir -p "$TESTDIR"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# Create a mock run first
mkdir -p "$TESTDIR/mock_fixtures"
for role in intake clarify route sketch propose gate critique repair judge rank deliver; do
  echo "{\"text\":\"{}\"}" > "$TESTDIR/mock_fixtures/${role}.json"
done

OUT=$("$BIN" run --mode fast --provider mock --prompt "test" --runs-dir "$TESTDIR" --non-interactive --mock-dir "$TESTDIR/mock_fixtures" 2>&1)
RUN_ID=$(echo "$OUT" | grep "run id:" | awk '{print $3}')
echo "Created run: $RUN_ID"

# Try to switch to a non-existent provider
ERR=$("$BIN" continue --run-id "$RUN_ID" --switch-provider NONEXISTENT --skip-checkpoint --runs-dir "$TESTDIR" 2>&1 || true)
echo "--- Switch to NONEXISTENT ---"
echo "$ERR"
if echo "$ERR" | grep -q "is not in the configured providers"; then
  echo "PASS: rejected unknown provider with clear error"
elif echo "$ERR" | grep -q "InvalidArgs"; then
  echo "PASS: rejected with InvalidArgs"
else
  echo "FAIL: did not reject unknown provider"
fi

# Try to switch to a valid provider (should not be rejected at this step)
ERR2=$("$BIN" continue --run-id "$RUN_ID" --switch-provider minimax --skip-checkpoint --runs-dir "$TESTDIR" 2>&1 || true)
echo "--- Switch to minimax ---"
echo "$ERR2" | head -3
if echo "$ERR2" | grep -q "is not in the configured providers"; then
  echo "FAIL: rejected valid provider minimax"
else
  echo "PASS: accepted valid provider minimax"
fi
