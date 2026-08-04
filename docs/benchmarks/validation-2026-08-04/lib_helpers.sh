#!/usr/bin/env bash
# Shared helpers for the validation harness. Sourced by every
# subagent's drive script. NOT a test runner — provides primitive
# operations only (start/stop proxy, run moagan, verify, log).
#
# Output contract: every helper prints one TSV line per action so the
# subagent can append to a single CSV report.

set -uo pipefail

ROOT="${ROOT:-/home/wolf/workspace/projects/moagan}"
BIN="${BIN:-${ROOT}/target/debug/moagan}"
REPORT_DIR="${REPORT_DIR:-/tmp/opencode/validation-2026-08-04}"
RUNS_DIR="${RUNS_DIR:-/tmp/opencode/validation-runs}"
mkdir -p "$REPORT_DIR" "$RUNS_DIR"

# Source .env once so all helpers see the API keys.
if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi
export MOAGAN_QUIET=1

# --- run_with_proxy <runs_dir> <upstream> <timeout>  → echoes port file path
# Starts the audit proxy sidecar in the background. The proxy listens
# on a kernel-assigned port (--port 0) and prints its local address
# to stdout. We parse the first line and expose PROXY_PORT globally.
#
# Uses a temp file for the proxy PID; cleans up on trap.
PROXY_PID=""
PROXY_PORT=""
PROXY_RUNS_DIR=""
start_proxy() {
  local runs_dir="$1"
  local upstream="$2"
  local timeout_secs="${3:-180}"
  local logfile
  logfile="$(mktemp "${REPORT_DIR}/proxy.XXXXXX.log")"
  PROXY_RUNS_DIR="$runs_dir"
  "$BIN" audit proxy \
    --upstream "$upstream" \
    --port 0 \
    --runs-dir "$runs_dir" \
    --timeout-secs "$timeout_secs" \
    > "$logfile" 2>&1 &
  PROXY_PID=$!
  for _ in $(seq 1 30); do
    if [[ -s "$logfile" ]] && grep -q "proxy listening" "$logfile"; then
      break
    fi
    sleep 0.5
  done
  PROXY_PORT="$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$logfile" | head -1 | sed 's/.*://')"
  if [[ -z "$PROXY_PORT" ]]; then
    return 1
  fi
  echo "$PROXY_PORT" > "${logfile}.port"
  echo "$logfile" > "${logfile}.log"
  return 0
}

stop_proxy() {
  if [[ -n "$PROXY_PID" ]]; then
    kill -TERM "$PROXY_PID" 2>/dev/null || true
    for _ in $(seq 1 20); do
      if ! kill -0 "$PROXY_PID" 2>/dev/null; then
        break
      fi
      sleep 0.5
    done
    kill -KILL "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
    PROXY_PORT=""
  fi
}

# --- run_moagan <runs_dir> <mode> <provider> <model> <prompt> <extra_args...>
# Runs `moagan run` with the proxy override (routes provider endpoint
# through the sidecar). Echoes the run id on stdout.
run_moagan() {
  local runs_dir="$1" mode="$2" provider="$3" model="$4" prompt="$5"
  shift 5
  local args=("$@")
  case "$provider" in
    minimax)      local endpoint="http://127.0.0.1:${PROXY_PORT}/anthropic/v1" ;;
    deepseek)     local endpoint="http://127.0.0.1:${PROXY_PORT}/v1" ;;
    opencode_go)  local endpoint="http://127.0.0.1:${PROXY_PORT}/v1" ;;
    mock|*)       local endpoint="" ;;
  esac
  local env_args=()
  if [[ -n "$endpoint" ]]; then
    env_args=("MOAGAN_MINIMAX_ENDPOINT=$endpoint" "env" "MOAGAN_MINIMAX_ENDPOINT=$endpoint")
  fi
  (
    if [[ -n "$endpoint" ]]; then
      export MOAGAN_MINIMAX_ENDPOINT="$endpoint"
    fi
    "$BIN" run \
      --mode "$mode" \
      --provider "$provider" \
      --model "$model" \
      --prompt "$prompt" \
      --runs-dir "$runs_dir" \
      --non-interactive \
      "${args[@]}"
  )
}

# --- verify_audit <runs_dir>  → echoes TSV stdout
# Runs `moagan audit verify` and echoes the TSV. Returns 0 on
# summary=ok, 1 on mismatch, 2 on invalid.
verify_audit() {
  local runs_dir="$1"
  "$BIN" audit verify --runs-dir "$runs_dir" 2>&1
}

# --- latest_run_id <runs_dir>  → echoes the most recent run id (full UUID)
latest_run_id() {
  local runs_dir="$1"
  # Read the full UUID directly from the runs directory. `moagan
  # inspect` only prints the short prefix, which is insufficient
  # for path construction.
  ls -1 "$runs_dir/.runs/" 2>/dev/null | sort | tail -1
}

# --- append_report <csv_file> <test_name> <status> <detail>
append_report() {
  local csv="$1" name="$2" status="$3" detail="$4"
  printf '%s\t%s\t%s\t%s\n' "$name" "$status" "$detail" "$(date -Iseconds)" >> "$csv"
}

# --- cleanup_all
cleanup_all() {
  stop_proxy
}
trap cleanup_all EXIT
