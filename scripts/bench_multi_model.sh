#!/usr/bin/env bash
# scripts/bench_multi_model.sh
#
# Reusable harness that runs the same prompt against a list of
# (provider, model) pairs and parses the resulting sidecars into a
# CSV row per run.
#
# Captured per row:
#   - provider, model
#   - wall-clock duration (s)        from telemetry/phases.jsonl.gz
#   - input_tokens, output_tokens    from meta.sqlite.calls table
#   - top-1 ranking score            from rankings/ranking.json
#   - sketch count                   count(sketches/*.json)
#   - proposal count                 count(proposals/*.json)
#   - portfolio path                 existence of final/portfolio.md
#   - error message                  empty on success, "code: msg" otherwise
#
# Required env:
#   MINIMAX_API_KEY          always loaded from .env
#   DEEPSEEK_API_KEY         needed for `--provider deepseek`
#   OPENCODE_GO_API_KEY      needed for `--provider opencode_go`
#   MOAGAN_MINIMAX_ENDPOINT  overridden to the real API endpoint when
#                            ~/.config/moagan/config.toml routes
#                            minimax through a local proxy (e.g. 8086).
#
# Optional env:
#   BIN                      path to moagan binary (default: ./target/debug/moagan)
#   TIMEOUT_S                per-run timeout (default: 300)
#   MOAGAN_BENCH_PROMPT      prompt to run (default: PCI-DSS prompt)
#   MOAGAN_BENCH_RUNS_DIR    parent for per-iter tmp dirs (default: /tmp/moagan-bench)
#
# The script does NOT call any API itself. It spawns `moagan run` and
# reads its on-disk sidecars.

set -euo pipefail

# ---- shell source files --------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---- env resolution ------------------------------------------------------

# 1) Load .env so MINIMAX_API_KEY is set.
if [[ -f "$REPO_ROOT/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "$REPO_ROOT/.env"
    set +a
fi

# 2) Override MOAGAN_MINIMAX_ENDPOINT if the operator's global config
#    routes minimax through a dead local proxy. Empty / unset means
#    "leave alone" — the default config wired
#    https://api.minimax.io/anthropic/v1 already serves real traffic.
if [[ -z "${MOAGAN_MINIMAX_ENDPOINT:-}" ]]; then
    export MOAGAN_MINIMAX_ENDPOINT="https://api.minimax.io/anthropic/v1"
fi

# 3) Required keys: warn (don't abort) for missing optional ones —
#    a row that needs deepseek key will just record an error.
: "${MINIMAX_API_KEY:=}"
: "${DEEPSEEK_API_KEY:=}"
: "${OPENCODE_GO_API_KEY:=}"

# ---- knobs ---------------------------------------------------------------

BIN="${BIN:-$REPO_ROOT/target/debug/moagan}"
TIMEOUT_S="${TIMEOUT_S:-300}"
PROMPT="${MOAGAN_BENCH_PROMPT:-Diseña un sistema de procesamiento de pagos online que sea seguro, escalable y cumpla con PCI-DSS}"
RUNS_PARENT="${MOAGAN_BENCH_RUNS_DIR:-/tmp/moagan-bench}"

# Hard-coded model roster for the Q8 multi-model comparison.
# Each entry is "<provider>|<model>". Operators can override the
# active list by setting MOAGAN_BENCH_MODELS to a newline-separated
# subset (same format). When unset/empty, the full Q8 roster is used.
MODELS=(
    "minimax|MiniMax-M3"
    "minimax|MiniMax-M2.7-highspeed"
    "minimax|MiniMax-M2.5"
    "deepseek|deepseek-v4-flash"
    "opencode_go|kimi-k2.7-code"
    "opencode_go|qwen3.7-max"
)
if [[ -n "${MOAGAN_BENCH_MODELS:-}" ]]; then
    MODELS=()
    while IFS= read -r line; do
        [[ -n "$line" && "$line" != \#* ]] && MODELS+=("$line")
    done <<<"$MOAGAN_BENCH_MODELS"
fi

# Verify the binary exists and is executable. Fail fast — better than
# later discovering `command not found` deep in a CSV row.
if [[ ! -x "$BIN" ]]; then
    echo "bench: moagan binary not found at $BIN" >&2
    echo "       build first: cargo build" >&2
    exit 2
fi

mkdir -p "$RUNS_PARENT"

# ---- csv output ----------------------------------------------------------

CSV="${RUNS_PARENT}/bench.csv"
: > "$CSV"
printf 'provider,model,duration_s,input_tokens,output_tokens,top1_score,sketches,proposals,portfolio_path,error\n' >> "$CSV"

# ---- per-run helper ------------------------------------------------------
#
# Runs one (provider, model) pair. Writes one CSV row. Never aborts the
# bench — a failure goes into the `error` column and execution continues
# with the next row.

run_one() {
    local pair="$1" provider model iter_dir
    provider="${pair%%|*}"
    model="${pair#*|}"

    iter_dir="$(mktemp -d "$RUNS_PARENT/run.${provider}-${model}.XXXXXX")"

    # Pick the right API key for each provider; surface the missing-key
    # case as a friendly error row instead of letting the binary
    # produce an unhelpful API error.
    local key_name=""
    case "$provider" in
        minimax)      key_name="MINIMAX_API_KEY"   ;;
        deepseek)     key_name="DEEPSEEK_API_KEY"  ;;
        opencode_go)  key_name="OPENCODE_GO_API_KEY" ;;
        mock)         key_name=""                  ;;
        *)            key_name=""                  ;;
    esac
    if [[ -n "$key_name" && -z "${!key_name:-}" ]]; then
        printf '%s,%s,,,,,,,,,error: missing %s\n' \
            "$provider" "$model" "$key_name" >> "$CSV"
        rm -rf "$iter_dir"
        return 0
    fi

    # Wall-clock stopwatch — captures the whole `moagan run` lifecycle
    # including startup, not just the LLM call timings inside phases.
    local started_unix ended_unix duration_s
    started_unix=$(date +%s)

    set +e
    local -a moagan_args=(
        run
        --mode standard
        --provider "$provider"
        --model "$model"
        --prompt "$PROMPT"
        --non-interactive
        --runs-dir "$iter_dir"
    )
    # Auto-wire the mock fixture path for the mock provider so the
    # parser test path doesn't need a separate code branch.
    if [[ "$provider" == "mock" && -n "${MOAGAN_BENCH_MOCK_DIR:-}" ]]; then
        moagan_args+=(--mock-dir "$MOAGAN_BENCH_MOCK_DIR")
    fi
    timeout --foreground "$TIMEOUT_S" "$BIN" "${moagan_args[@]}" \
        >"$iter_dir/stdout.txt" 2>"$iter_dir/stderr.txt"
    local exit_code=$?
    set -e

    ended_unix=$(date +%s)
    duration_s=$((ended_unix - started_unix))

    if [[ $exit_code -ne 0 ]]; then
        # Tail the binary's stderr into the error column so the report
        # can quote it. Cap at 200 chars to keep the CSV readable.
        local err_msg
        err_msg=$(tail -c 800 "$iter_dir/stderr.txt" 2>/dev/null \
            | tr -d '\n' \
            | tr ',' ';' \
            | head -c 200)
        local label
        case $exit_code in
            124) label="timeout(${TIMEOUT_S}s)" ;;
            *)   label="exit($exit_code)" ;;
        esac
        printf '%s,%s,%s,,,,,,,%s:%s\n' \
            "$provider" "$model" "$duration_s" "$label" "$err_msg" >> "$CSV"
        return 0
    fi

    # Locate the freshly-minted run id; moagan prints `run id: <uuid>`
    # on stdout. Fall back to "ls -t .runs/" in case of whitespace.
    local run_id=""
    run_id=$(grep -E '^run id: ' "$iter_dir/stdout.txt" 2>/dev/null \
        | tail -1 \
        | sed 's/^run id: //' \
        | tr -d '[:space:]')
    if [[ -z "$run_id" && -d "$iter_dir/.runs" ]]; then
        run_id=$(find "$iter_dir/.runs" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' 2>/dev/null \
            | sort -nr \
            | head -1 \
            | awk '{print $2}' \
            | xargs -r basename)
    fi

    if [[ -z "$run_id" || ! -d "$iter_dir/.runs/$run_id" ]]; then
        printf '%s,%s,%s,,,,,,,no_run_dir_found\n' \
            "$provider" "$model" "$duration_s" >> "$CSV"
        return 0
    fi

    local run_dir="$iter_dir/.runs/$run_id"
    local db="$iter_dir/meta.sqlite"
    local err_cell="" input_tokens output_tokens top1 sketches proposals portfolio

    if [[ ! -f "$db" ]]; then
        err_cell="no_meta_sqlite"
    fi

    # Tokens from the `calls` table (per-run; provider_rollups is
    # cross-run aggregate and would be wrong when these tmpdirs share
    # a meta DB, which they don't here).
    if [[ -z "$err_cell" ]]; then
        read -r input_tokens output_tokens < <(sqlite3 -separator ' ' "$db" \
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0) \
             FROM calls WHERE run_id='$run_id'")
        : "${input_tokens:=0}"
        : "${output_tokens:=0}"
    else
        input_tokens=0
        output_tokens=0
    fi

    # Top-1 score from rankings/ranking.json.
    if [[ -z "$err_cell" ]]; then
        top1=$(jq -r '.ranked[0].score // empty' \
            "$run_dir/rankings/ranking.json" 2>/dev/null || true)
        : "${top1:=}"

        # Drop the leading "0:" bash arithmetic quirks from .score.
        # jq prints 8.4 as 8.4 fine; nothing to do.
    else
        top1=""
    fi

    # Counts of sketches/ proposals — sketches/ is only produced on
    # modes that have a sketch phase; standard-mode runs DO create it,
    # so this is the canonical signal.
    if [[ -z "$err_cell" ]]; then
        sketches=$(find "$run_dir/sketches" -maxdepth 1 -type f -name '*.json' 2>/dev/null | wc -l | tr -d ' ')
        proposals=$(find "$run_dir/proposals" -maxdepth 1 -type f -name 'p_*.json' 2>/dev/null | wc -l | tr -d ' ')
    else
        sketches=0
        proposals=0
    fi

    if [[ -f "$run_dir/final/portfolio.md" ]]; then
        portfolio="yes"
    else
        portfolio="no"
    fi

    # Escape commas in any free-text column by replacing with semicolons.
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$provider" "$model" "$duration_s" \
        "$input_tokens" "$output_tokens" \
        "${top1:-}" "$sketches" "$proposals" \
        "$portfolio" "${err_cell:-}" >> "$CSV"
}

# ---- main loop -----------------------------------------------------------

if command -v jq >/dev/null 2>&1; then
    : # jq available, use it
elif command -v python3 >/dev/null 2>&1; then
    # Shell out to python3 for the rankings/ranking.json read by aliasing
    # `jq` to a python one-liner below.
    shopt -s expand_aliases
    alias jq='python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get(\"ranked\",[{}])[0].get(\"score\",\"\")) if len(d.get(\"ranked\",[]))>0 else print(\"\")"'
fi

echo "bench: $BIN, mode=standard, timeout=${TIMEOUT_S}s, runs-dir=$RUNS_PARENT" >&2
echo "bench: models = ${#MODELS[@]}" >&2

for pair in "${MODELS[@]}"; do
    echo "bench: running $pair ..." >&2
    run_one "$pair"
done

# ---- csv dump ------------------------------------------------------------

echo "==== $CSV ===="
cat "$CSV"
