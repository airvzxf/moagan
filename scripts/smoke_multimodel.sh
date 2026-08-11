#!/usr/bin/env bash
# Moagan multi-model comparison. Runs the same prompt against each of
# the four M-series models and records timing, artefacts, and the
# final portfolio content for review.
set -euo pipefail

if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

: "${MINIMAX_API_KEY:?MINIMAX_API_KEY not set}"
: "${MOAGAN_HOME:=/home/wolf/.local/share/moagan}"
export MOAGAN_HOME

# Skip the per-(provider,model) max_tokens auto-probe. The four
# minimax-m* models are well known to the developer already, and the
# probe would add ~30 sequential HTTP calls for every model at first
# startup. CI is the right place to amortise that cost.
export MOAGAN_MAX_TOKEN_AUTO=false
export MOAGAN_MAX_TOKEN_AUTO_SAVE=false

# Wipe so we start clean and inspect shows only this run's results.
rm -rf "$MOAGAN_HOME"
mkdir -p "$MOAGAN_HOME"

BIN=./target/release/moagan
readonly Prompt='Enumera los 7 colores del arcoíris en orden canónico (ROYGBIV) y dame un consejo mnemotécnico.'
Models=(
    "minimax-m3:MiniMax-M3 (M3, agentic SOTA, 1M context)"
    "minimax-m2.7:MiniMax-M2.7 (M2.7, 204.8K context)"
    "minimax-m2.7-highspeed:MiniMax-M2.7-highspeed (M2.7 highspeed)"
    "minimax-m2.5:MiniMax-M2.5 (M2.5)"
)
TSV=/tmp/moagan-multimodel.tsv
: > "$TSV"
printf 'provider\tdescription\tduration_s\tstatus\trun_id\n' >> "$TSV"

run_one() {
    local provider="$1" desc="$2"
    local started ended dur status run_id
    started=$(date +%s)
    if output=$("$BIN" run --mode fast --provider "$provider" --prompt "$Prompt" 2>&1); then
        status=ok
    else
        status=err
    fi
    ended=$(date +%s)
    dur=$((ended - started))
    LATEST=$(ls -t "$MOAGAN_HOME/.runs" 2>/dev/null | head -1)
    if [[ -n "$LATEST" && -d "$MOAGAN_HOME/.runs/$LATEST" ]]; then
        run_id="$LATEST"
    else
        run_id="-"
    fi
    printf '%s\t%s\t%s\t%s\t%s\n' "$provider" "$desc" "$dur" "$status" "$run_id" >> "$TSV"
    printf '[%s] dur=%ss status=%s run_id=%s\n' "$provider" "$dur" "$status" "$run_id"
}

for entry in "${Models[@]}"; do
    provider="${entry%%:*}"
    desc="${entry#*:}"
    run_one "$provider" "$desc"
done

echo
echo "==== /tmp/moagan-multimodel.tsv ===="
cat "$TSV"

echo
echo "==== moagan inspect ===="
"$BIN" inspect --limit 10

echo
echo "==== run ids and portfolios ===="
sqlite3 "$MOAGAN_HOME/meta.sqlite" \
    "SELECT run_id, mode, status FROM runs ORDER BY created_unix DESC;" 2>/dev/null

echo
for run_id in $(sqlite3 "$MOAGAN_HOME/meta.sqlite" "SELECT run_id FROM runs ORDER BY created_unix;" 2>/dev/null); do
    echo "--- $run_id ---"
    if [[ -f "$MOAGAN_HOME/.runs/$run_id/final/portfolio.md" ]]; then
        head -20 "$MOAGAN_HOME/.runs/$run_id/final/portfolio.md"
    else
        echo "(no portfolio.md)"
    fi
    echo
done
