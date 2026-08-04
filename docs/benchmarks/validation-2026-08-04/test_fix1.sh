#!/usr/bin/env bash
# Verify response_format=json_object is sent for OpenAI-compat providers.
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-runs/fix1-$$"
mkdir -p "$TESTDIR"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# Patch config temporarily to use proxy endpoint for deepseek
cat > "$TESTDIR/config.toml" << 'CFG'
[providers.deepseek]
kind = "deepseek"
endpoint = "http://127.0.0.1:__PORT__/v1"
model = "deepseek-v4-flash"
hard_incompatibilities = []
CFG

# Start proxy
PROXY_LOG="$TESTDIR/proxy.log"
"$BIN" audit proxy --upstream https://api.deepseek.com/v1 --port 0 --runs-dir "$TESTDIR" > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 3
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
echo "proxy on port $PROXY_PORT"

# Hot-replace the endpoint in the temp config
sed -i "s|__PORT__|$PROXY_PORT|" "$TESTDIR/config.toml"

# Run with deepseek via proxy
export MOAGAN_CONFIG="$TESTDIR/config.toml"
timeout 90 "$BIN" run --mode fast --provider deepseek --model deepseek-v4-flash \
  --prompt "Write JSON: {\"x\":1}" --non-interactive \
  --runs-dir "$TESTDIR" > "$TESTDIR/run.log" 2>&1

kill -TERM $PROXY_PID 2>/dev/null || true
wait $PROXY_PID 2>/dev/null || true

# Check the audit log
RUN_ID=$(ls -t "$TESTDIR/.runs/" 2>/dev/null | head -1)
echo "Run id: $RUN_ID"
if [[ -n "$RUN_ID" && -f "$TESTDIR/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" ]]; then
  echo "--- body_canonical on first request ---"
  gunzip -c "$TESTDIR/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" | head -1 | python3 -c 'import json, sys; d = json.loads(sys.stdin.read()); print(d.get("body_canonical"))'
  echo "--- response_format occurrences ---"
  gunzip -c "$TESTDIR/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" | grep -c '"response_format"'
fi
