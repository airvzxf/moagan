#!/usr/bin/env bash
# Probe Pasada 1: 18 modelos OpenCode Go — connectivity + mode=fast.
# Ejecuta contra la API real. Captura resultados en CSV.
set -uo pipefail

BIN="/home/wolf/workspace/projects/moagan/target/debug/moagan"
ROOT="/home/wolf/workspace/projects/moagan"
TESTDIR="/tmp/opencode/validation-2026-08-04/probe-v2-fast"
mkdir -p "$TESTDIR"
CSV="$TESTDIR/results.csv"
: > "$CSV"
printf 'model\tep_path\tep_kind\tresult\tdetail\ttimestamp\n' >> "$CSV"

set -a; source "${ROOT}/.env"; set +a
export MOAGAN_QUIET=1

# Start audit proxy (upstream is OpenCode Go /v1 endpoint)
PROXY_LOG="$TESTDIR/proxy.log"
"$BIN" audit proxy \
  --upstream "https://opencode.ai/zen/go/v1" \
  --port 0 \
  --runs-dir "$TESTDIR" > "$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 3
PROXY_PORT=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$PROXY_LOG" | head -1 | sed 's/.*://')
if [[ -z "$PROXY_PORT" ]]; then
  echo "ERROR: proxy failed to start"
  cat "$PROXY_LOG"
  exit 1
fi
echo "proxy on port $PROXY_PORT"

# 18 models with their endpoint paths and wire-format kinds
# Format: model|path|kind
MODELS=(
  "gpt-5.6-luna|responses|responses"
  "glm-5.2|chat/completions|chat"
  "glm-5.1|chat/completions|chat"
  "kimi-k3|chat/completions|chat"
  "kimi-k2.7-code|chat/completions|chat"
  "kimi-k2.6|chat/completions|chat"
  "deepseek-v4-pro|chat/completions|chat"
  "deepseek-v4-flash|chat/completions|chat"
  "mimo-v2.5|chat/completions|chat"
  "mimo-v2.5-pro|chat/completions|chat"
  "minimax-m3|messages|anthropic"
  "minimax-m2.7|messages|anthropic"
  "minimax-m2.5|messages|anthropic"
  "qwen3.8-max|messages|anthropic"
  "qwen3.7-max|messages|anthropic"
  "qwen3.7-plus|messages|anthropic"
  "qwen3.6-plus|messages|anthropic"
  "hy3|chat/completions|chat"
)

run_one() {
  local pair="$1"
  local model="${pair%%|*}"
  local rest="${pair#*|}"
  local path="${rest%%|*}"
  local kind="${rest##*|}"
  local subdir="$TESTDIR/fast-$model"
  local prompt_text="Diseña una API REST minimal para gestionar tareas. Responde con estructura JSON."
  local csv_line=""

  # The proxy listens at /v1, so the path we want the request to hit is
  # /v1/<path>. The proxy is bound to https://opencode.ai/zen/go/v1/<path>.
  # We rewrite MOAGAN_MINIMAX_ENDPOINT to point to the proxy; the gateway
  # at opencode.ai uses the same path so the URL becomes
  # http://127.0.0.1:PORT/<path>.
  # We can't use MOAGAN_MINIMAX_ENDPOINT for opencode_go (it doesn't
  # honor that env var). Instead, configure a custom provider via
  # MOAGAN_CONFIG that points to the proxy.

  # For OpenCode Go the easiest path is to set MOAGAN_OPENCODE_GO_ENDPOINT
  # if we add one, but we don't have that. So we run direct, no proxy,
  # with timeout 90s. The audit-proxy detours lose fidelity because the
  # provider resolves the endpoint string internally.
  local started; started=$(date +%s)
  local out=""
  local pm_out
  pm_out=$(timeout 600 "$BIN" run \
    --mode fast \
    --provider opencode_go \
    --model "$model" \
    --prompt "$prompt_text" \
    --non-interactive \
    --runs-dir "$subdir" 2>&1)
  local rc=$?
  local ended; ended=$(date +%s)
  local wall=$((ended - started))
  local run_id; run_id=$(echo "$pm_out" | grep "run id:" | awk '{print $3}' | head -1)
  local result; result="ERROR"
  local detail="$pm_out"

  # Categorize result
  if [[ $rc -eq 0 ]]; then
    if [[ -n "$run_id" ]] && [[ -f "$subdir/.runs/$run_id/final/portfolio.md" || -f "$subdir/.runs/$run_id/rankings/ranking.json" ]]; then
      result="PASS"
      detail="wall=${wall}s run_id=$run_id"
    else
      result="PARTIAL"
      detail="rc=0 but no portfolio/ranking: wall=${wall}s run_id=$run_id"
    fi
  elif echo "$pm_out" | grep -qE "SchemaViolation"; then
    result="SCHEMA_VIOLATION"
    detail=$(echo "$pm_out" | grep -E "schema violation" | head -1 | cut -c1-200)
  elif echo "$pm_out" | grep -qE "invalid temperature|only 1 is allowed"; then
    result="TEMP_REJECTED"
    detail=$(echo "$pm_out" | grep -E "invalid temperature" | head -1 | cut -c1-200)
  elif echo "$pm_out" | grep -qE "ModelNotFound|model not found"; then
    result="MODEL_NOT_FOUND"
    detail=$(echo "$pm_out" | grep -E "ModelNotFound|model" | head -1 | cut -c1-200)
  elif echo "$pm_out" | grep -qE "decode: error decoding"; then
    result="DECODE_FAIL"
    detail="decode failure (likely non-OpenAI-compat response shape)"
  elif echo "$pm_out" | grep -qE "Timeout|TIMEOUT|timed out"; then
    result="TIMEOUT"
    detail="wall=${wall}s"
  elif echo "$pm_out" | grep -qE "InvalidApiKey"; then
    result="KEY_MISSING"
    detail="OPENCODE_GO_API_KEY not set"
  else
    result="OTHER_ERR"
    detail=$(echo "$pm_out" | head -5 | tr '\n' ' ' | cut -c1-200)
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$model" "$path" "$kind" "$result" "$detail" "$(date -Iseconds)" >> "$CSV"
  echo "$model [$kind/$path]: $result ($detail)"
}

for m in "${MODELS[@]}"; do
  run_one "$m"
done

kill -TERM $PROXY_PID 2>/dev/null || true
wait $PROXY_PID 2>/dev/null || true

echo "---"
echo "Results CSV: $CSV"
column -t -s $'\t' "$CSV"
