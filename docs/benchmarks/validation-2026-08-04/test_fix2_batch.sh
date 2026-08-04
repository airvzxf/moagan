#!/usr/bin/env bash
# Verify alias resolution works across all 4 MiniMax variants
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-runs/fix2-batch-$$"
mkdir -p "$TESTDIR"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

declare -A EXPECTED=(
    ["minimax-m3"]="MiniMax-M3"
    ["minimax-m2.7"]="MiniMax-M2.7"
    ["minimax-m2.7-highspeed"]="MiniMax-M2.7-highspeed"
    ["minimax-m2.5"]="MiniMax-M2.5"
)

# Patch config to use proxy
PROXY_LOG="$TESTDIR/proxy.log"
"$BIN" audit proxy --upstream https://api.minimax.io/anthropic/v1 --port 0 --runs-dir "$TESTDIR" > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 3
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
echo "proxy on port $PROXY_PORT"
export MOAGAN_MINIMAX_ENDPOINT="http://127.0.0.1:$PROXY_PORT/anthropic/v1"

for alias in "${!EXPECTED[@]}"; do
    expected="${EXPECTED[$alias]}"
    subdir="$TESTDIR/$alias"
    mkdir -p "$subdir"
    timeout 25 "$BIN" run --mode fast --provider minimax --model "$alias" \
      --prompt "echo" --non-interactive \
      --runs-dir "$subdir" > "$subdir/run.log" 2>&1
    AUDIT_FILE=$(find "$subdir/.runs/" -name "external_audit.jsonl.gz" 2>/dev/null | head -1)
    if [[ -n "$AUDIT_FILE" ]]; then
        FOUND=$(gunzip -c "$AUDIT_FILE" | head -1 | python3 -c 'import json, sys; d = json.loads(sys.stdin.read()); body = d.get("body_canonical") or ""; import re; m = re.findall(r"\"model\":\s*\"([^\"]+)\"", body); print(m[0] if m else "?")')
        if [[ "$FOUND" == "$expected" ]]; then
            echo "PASS: --model $alias → $expected"
        else
            echo "FAIL: --model $alias → expected $expected, got $FOUND"
        fi
    else
        echo "FAIL: --model $alias → no audit log"
    fi
done

kill -TERM $PROXY_PID 2>/dev/null || true
wait $PROXY_PID 2>/dev/null || true
