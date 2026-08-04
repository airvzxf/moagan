#!/usr/bin/env bash
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-2026-08-04/probe-v2-inspect"
mkdir -p "$TESTDIR"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# Pick a model that returned empty content to investigate
MODEL="qwen3.7-max"
subdir="$TESTDIR/inspect-$MODEL"
mkdir -p "$subdir"

PROXY_LOG="$subdir/proxy.log"
"$BIN" audit proxy \
  --upstream "https://opencode.ai/zen/go/v1" \
  --port 0 \
  --runs-dir "$subdir" > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 2
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')

# Inject proxy via config
cat > "$subdir/config.toml" <<TOML
[providers.opencode_go]
kind = "opencode_go"
endpoint = "http://127.0.0.1:$PROXY_PORT"
model = "x"
TOML

# Run with timeout
MOAGAN_CONFIG="$subdir/config.toml" timeout 90 "$BIN" run \
  --mode fast \
  --provider opencode_go \
  --model "$MODEL" \
  --prompt "Diseña una API REST minimal para gestionar tareas. Responde con estructura JSON." \
  --non-interactive \
  --runs-dir "$subdir" > "$subdir/run.log" 2>&1

kill -TERM $PROXY_PID 2>/dev/null || true
wait $PROXY_PID 2>/dev/null || true

# Show the URL + response pairs from the audit log
RUN_ID=$(ls -t "$subdir/.runs/" 2>/dev/null | head -1)
echo "=== $MODEL (run_id=$RUN_ID) ==="
gunzip -c "$subdir/.runs/$RUN_ID/telemetry/external_audit.jsonl.gz" | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        d = json.loads(line)
    except:
        continue
    event = d.get("event")
    url = d.get("url")
    status = d.get("status")
    body_len = len(d.get("body_canonical") or "")
    body = (d.get("body_canonical") or "")[:200]
    print("event=%s url=%s status=%s body_len=%d" % (event, url, status, body_len))
    if event == "response":
        print("--- body snippet ---")
        print(body)
        print("--- end ---")
' | head -10
