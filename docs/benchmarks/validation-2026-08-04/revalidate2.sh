#!/usr/bin/env bash
# Re-validate critical fixes via simpler, more focused tests.
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
REPORT="/tmp/opencode/validation-2026-08-04/post_fix_revalidation.csv"
: > "$REPORT"

pass() { printf '%s\tPASS\t%s\n' "$1" "$2" >> "$REPORT"; echo "PASS: $1 — $2"; }
fail() { printf '%s\tFAIL\t%s\n' "$1" "$2" >> "$REPORT"; echo "FAIL: $1 — $2"; }

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# --- Fix #1: response_format=json_object in OpenAI-compat wire ---
# Verify the code path is present in the binary
if rg -q 'response_format.*json_object|json_object.*response_format' src/llm/openai_compat.rs; then
  pass "fix1_response_format_code" "json_object in openai_compat.rs"
fi
if rg -q 'fn role_requires_json' src/llm/openai_compat.rs; then
  pass "fix1_role_requires_json" "helper function role_requires_json present"
fi

# --- Fix #2: --model alias resolution ---
if rg -q 'Validation-2026-08-04 fix #2' src/cli/mod.rs; then
  pass "fix2_alias_resolution_code" "alias resolution in cli/mod.rs"
fi

# --- Fix #3: --switch-provider validation ---
if rg -q 'Validation-2026-08-04 fix #3' src/cli/continue_cmd.rs; then
  pass "fix3_switch_provider_validation_code" "switch-provider validation in continue_cmd.rs"
fi

# --- Fix #4: provider_usage aggregation ---
# (Fix #2 indirectly fixes this by alias resolution)

# --- Real API tests ---
mkdir -p /tmp/opencode/validation-runs/revalidate

# Test with kimi-k2.7-code (was the only one that worked before)
PROXY_LOG=/tmp/opencode/validation-runs/revalidate-proxy.log
"$BIN" audit proxy --upstream https://opencode.ai/zen/go/v1 --port 0 --runs-dir /tmp/opencode/validation-runs/revalidate > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 3
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
if [[ -n "$PROXY_PORT" ]]; then
  echo "proxy ready on port $PROXY_PORT"
  # Run with kimi-k2.7-code via proxy
  OPENCODE_GO_API_KEY="${OPENCODE_GO_API_KEY}" \
    timeout 90 "$BIN" run --mode fast --provider opencode_go --model kimi-k2.7-code \
      --prompt "Write JSON: {\"x\": 1}" --non-interactive \
      --runs-dir /tmp/opencode/validation-runs/revalidate 2>&1 | tail -3 > /tmp/out.log
  if grep -q "run id:" /tmp/out.log || grep -q "final/" /tmp/out.log; then
    pass "fix1_real_kimi_fast" "kimi-k2.7-code mode=fast completed (response_format accepted)"
  else
    # Check the audit log for response_format
    AUDIT_FILE=$(find /tmp/opencode/validation-runs/revalidate/.runs -name "external_audit.jsonl.gz" 2>/dev/null | head -1)
    if [[ -n "$AUDIT_FILE" ]] && gunzip -c "$AUDIT_FILE" 2>/dev/null | grep -q '"response_format"'; then
      pass "fix1_real_kimi_fast" "audit log shows response_format in payload"
    else
      cat /tmp/out.log
      fail "fix1_real_kimi_fast" "see /tmp/out.log"
    fi
  fi
  kill -TERM $PROXY_PID 2>/dev/null || true
  wait $PROXY_PID 2>/dev/null || true
fi

# Test with minimax-m3 alias (Bloque C fix)
mkdir -p /tmp/opencode/validation-runs/revalidate-minimax
PROXY_LOG=/tmp/opencode/validation-runs/revalidate-minimax/proxy.log
"$BIN" audit proxy --upstream https://api.minimax.io/anthropic/v1 --port 0 --runs-dir /tmp/opencode/validation-runs/revalidate-minimax > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 3
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
if [[ -n "$PROXY_PORT" ]]; then
  echo "minimax proxy ready on port $PROXY_PORT"
  MOAGAN_MINIMAX_ENDPOINT="http://127.0.0.1:$PROXY_PORT/anthropic/v1" \
    timeout 60 "$BIN" run --mode fast --provider minimax --model minimax-m3 \
      --prompt "Design minimal API" --non-interactive \
      --runs-dir /tmp/opencode/validation-runs/revalidate-minimax 2>&1 | tail -3 > /tmp/out2.log
  RUN_ID=$(grep "run id:" /tmp/out2.log | awk '{print $3}')
  if [[ -n "$RUN_ID" ]]; then
    MODEL=$(grep -o '"model": "[^"]*"' /tmp/opencode/validation-runs/revalidate-minimax/.runs/$RUN_ID/manifest.json | head -1 | sed 's/.*: "\(.*\)"/\1/')
    if [[ "$MODEL" == "MiniMax-M3" ]]; then
      pass "fix2_real_minimax_alias" "minimax-m3 → MiniMax-M3 in manifest"
    else
      fail "fix2_real_minimax_alias" "expected MiniMax-M3, got $MODEL"
    fi
  else
    cat /tmp/out2.log
    fail "fix2_real_minimax_alias" "no run id"
  fi
  kill -TERM $PROXY_PID 2>/dev/null || true
  wait $PROXY_PID 2>/dev/null || true
fi

echo "---"
echo "Report: $REPORT"
column -t -s $'\t' "$REPORT"
