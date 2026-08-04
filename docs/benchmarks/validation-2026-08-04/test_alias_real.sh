#!/usr/bin/env bash
# Focused test: ensure --model minimax-m3 resolves to MiniMax-M3 in the wire payload.
# Use the proxy + MOAGAN_MINIMAX_ENDPOINT override to route through the audit sidecar.
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-runs/alias-test-$$"
mkdir -p "$TESTDIR"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# Start proxy
PROXY_LOG="$TESTDIR/proxy.log"
"$BIN" audit proxy --upstream https://api.minimax.io/anthropic/v1 --port 0 --runs-dir "$TESTDIR" > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 3
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
echo "proxy on port $PROXY_PORT"

# Run with --model minimax-m3 (the alias form) AND MOAGAN_MINIMAX_ENDPOINT override
export MOAGAN_MINIMAX_ENDPOINT="http://127.0.0.1:$PROXY_PORT/anthropic/v1"
timeout 30 "$BIN" run --mode fast --provider minimax --model minimax-m3 \
  --prompt "echo" --non-interactive \
  --runs-dir "$TESTDIR" > "$TESTDIR/run.log" 2>&1

kill -TERM $PROXY_PID 2>/dev/null || true
wait $PROXY_PID 2>/dev/null || true

# Find the run id
RUN_ID=$(grep "run id:" "$TESTDIR/run.log" | awk '{print $3}')
echo "run id: $RUN_ID"

# Look at the audit log
if [[ -n "$RUN_ID" ]]; then
  AUDIT_FILE="$TESTDIR/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz"
  if [[ -f "$AUDIT_FILE" ]]; then
    MODEL_LINE=$(gunzip -c "$AUDIT_FILE" | head -1 | grep -o '"model":"[^"]*"' | head -1)
    echo "First request model field: $MODEL_LINE"
    if [[ "$MODEL_LINE" == '"model":"MiniMax-M3"' ]]; then
      echo "PASS: alias minimax-m3 → MiniMax-M3 in wire payload"
    else
      echo "FAIL: expected MiniMax-M3, got $MODEL_LINE"
    fi
  else
    echo "no audit file"
    cat "$TESTDIR/run.log" | tail -5
  fi
fi

# Inspect the manifest
if [[ -n "$RUN_ID" ]] && [[ -f "$TESTDIR/.runs/$RUN_ID/manifest.json" ]]; then
  MANIFEST_MODEL=$(grep -o '"model": "[^"]*"' "$TESTDIR/.runs/$RUN_ID/manifest.json" | head -1)
  echo "Manifest model: $MANIFEST_MODEL"
fi
