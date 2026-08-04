#!/usr/bin/env bash
# Probe Pasada 1.5: investigate failing models with audit proxy
# to capture raw response bodies.
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-2026-08-04/probe-v2-raw"
mkdir -p "$TESTDIR"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# Pick the top 5 most informative failures
MODELS=(qwen3.8-max qwen3.7-max glm-5.2 kimi-k2.7-code hy3)

for model in "${MODELS[@]}"; do
  subdir="$TESTDIR/raw-$model"
  mkdir -p "$subdir"
  PROXY_LOG="$subdir/proxy.log"
  "$BIN" audit proxy \
    --upstream "https://opencode.ai/zen/go/v1" \
    --port 0 \
    --runs-dir "$subdir" > "$PROXY_LOG" 2>&1 &
  PROXY_PID=$!
  sleep 2
  PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
  
  # We can't easily override the opencode_go endpoint, so we have to
  # inject the proxy via the config.toml trick.
  CONFIG_FILE="$subdir/config.toml"
  cat > "$CONFIG_FILE" <<TOML
[providers.opencode_go]
kind = "opencode_go"
endpoint = "http://127.0.0.1:$PROXY_PORT"
model = "x"
TOML
  
  MOAGAN_CONFIG="$CONFIG_FILE" timeout 90 "$BIN" run \
    --mode fast \
    --provider opencode_go \
    --model "$model" \
    --prompt "Diseña una API REST minimal para gestionar tareas. Responde con estructura JSON." \
    --non-interactive \
    --runs-dir "$subdir" > "$subdir/run.log" 2>&1
  
  kill -TERM $PROXY_PID 2>/dev/null || true
  wait $PROXY_PID 2>/dev/null || true
  
  # Inspect the audit log
  RUN_ID=$(ls -t "$subdir/.runs/" 2>/dev/null | head -1)
  if [[ -n "$RUN_ID" && -f "$subdir/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" ]]; then
    echo "=== $model (run_id=$RUN_ID) ==="
    # Show the first request body
    echo "--- First request body ---"
    gunzip -c "$subdir/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" | head -1 | python3 -c '
import json, sys
d = json.loads(sys.stdin.read())
body = d.get("body_canonical") or ""
print(body[:600])
print("...")
print(body[-200:] if len(body) > 600 else "")
'
    echo "--- Last response body ---"
    LAST_LINE=$(gunzip -c "$subdir/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" | tail -1)
    echo "$LAST_LINE" | python3 -c '
import json, sys
d = json.loads(sys.stdin.read())
body = d.get("body_canonical") or ""
print(body[:600])
print("...")
print(body[-200:] if len(body) > 600 else "")
' 2>/dev/null
    echo ""
  fi
done
