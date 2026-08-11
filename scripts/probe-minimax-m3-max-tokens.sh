#!/usr/bin/env bash
# Find the upstream max_tokens ceiling for one or more MiniMax models by
# probing the Anthropic-compatible endpoint at
# https://api.minimax.io/anthropic/v1/messages.
#
# Strategy (per model):
#   1. Phase 1     — exponential search 2^10..2^25 (sequential POSTs).
#                    Stop at the first HTTP 400 that carries the
#                    max_tokens signature.
#   1.5 Quick probe — test `lo + 1`. Most providers reject it outright
#                    (sharp boundary at the largest accepted value), so
#                    we skip Phase 2 entirely.
#   2. Phase 2     — divide y vencerás on [lo+1, hi-1] until hi-lo <= 1.
#   3. Verify `lo + 1` is rejected (skipped if already probed above).
#
# The MINIMAX_API_KEY is loaded from .env via grep+cut (the file may
# carry unrelated vars, so it is never `source`'d). Nothing is written
# to the repo or to ~/.local/share/moagan/.
#
# Usage:
#   scripts/probe-minimax-m3-max-tokens.sh
#     Default matrix: MiniMax-M3 MiniMax-M2.7 MiniMax-M2.7-highspeed MiniMax-M2.5
#
#   PROBE_MINIMAX_MODELS="MiniMax-M3,MiniMax-M2.5" scripts/probe-minimax-m3-max-tokens.sh
#     Override the matrix with a comma-separated list of model names.
#
#   PROBE_MINIMAX_ENV_FILE=/path/to/.env scripts/probe-minimax-m3-max-tokens.sh
#     Use a different .env file.
#
#   MOAGAN_MINIMAX_ENDPOINT=https://other.example/anthropic/v1 scripts/probe-minimax-m3-max-tokens.sh
#     Probe a different Anthropic-compatible upstream.
set -euo pipefail

ENV_FILE="${PROBE_MINIMAX_ENV_FILE:-/home/wolf/workspace/projects/moagan/.env}"
ENDPOINT_BASE="${MOAGAN_MINIMAX_ENDPOINT:-https://api.minimax.io/anthropic/v1}"
EXP_LO=10
EXP_HI=25
CURL_TIMEOUT=10
INDETERMINATE_RETRIES=1

# Model matrix. Default is the four direct MiniMax models the operator
# roster exposes today. Override with PROBE_MINIMAX_MODELS="A,B,C" if
# the operator wants a different subset.
DEFAULT_MODELS=(MiniMax-M3 MiniMax-M2.7 MiniMax-M2.7-highspeed MiniMax-M2.5)
if [ -n "${PROBE_MINIMAX_MODELS:-}" ]; then
    # Split on comma, trim each side with sed, build the final array.
    # `mapfile -d ,` reads comma-delimited lines; we then trim each one.
    mapfile -d , -t RAW_MODELS <<< "$PROBE_MINIMAX_MODELS,"
    MODELS=()
    for raw in "${RAW_MODELS[@]}"; do
        # Drop the trailing empty element `mapfile` leaves when the
        # input ends with the delimiter, and trim whitespace on both
        # sides. Trailing newlines from mapfile are stripped first.
        trimmed="$(printf '%s' "$raw" | tr -d '\n' | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
        if [ -n "$trimmed" ]; then
            MODELS+=("$trimmed")
        fi
    done
else
    MODELS=("${DEFAULT_MODELS[@]}")
fi

ts() { date +'%H:%M:%S'; }
log() { printf '[%s] [probe] %s\n' "$(ts)" "$*" >&2; }
err() { printf '[%s] [probe] ERROR: %s\n' "$(ts)" "$*" >&2; }
sep() { printf '[%s] [probe] %s\n' "$(ts)" "$*" >&2; }

# Load the API key from .env without executing anything else.
KEY="$(grep -E '^[[:space:]]*MINIMAX_API_KEY[[:space:]]*=' "$ENV_FILE" \
        | head -n1 \
        | cut -d= -f2- \
        | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
        | sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'$//")"
if [ -z "$KEY" ]; then
    err "MINIMAX_API_KEY is empty or missing in $ENV_FILE"
    exit 1
fi
# Sanity: MiniMax Coding Plan keys start with sk-cp- and are ~125 chars.
if [ "${#KEY}" -lt 20 ]; then
    err "MINIMAX_API_KEY looks too short (${#KEY} chars); aborting"
    exit 1
fi

URL="${ENDPOINT_BASE%/}/messages"

# classify <http_code> <body>
# Emits a single token on stdout: ACCEPTED | REJECTED | INDETERMINATE
classify() {
    local code="$1" body="$2"
    if [ "$code" -ge 200 ] && [ "$code" -lt 300 ]; then
        echo "ACCEPTED"; return
    fi
    if [ "$code" -eq 400 ] || [ "$code" -eq 413 ] || [ "$code" -eq 422 ]; then
        if printf '%s' "$body" | grep -qiE 'max[_ ]tokens|max[_ ]tokens[_ ]override|tokens limit|maximum context length|does not support max tokens'; then
            echo "REJECTED"; return
        fi
    fi
    echo "INDETERMINATE"
}

# probe <n>
# Returns 0=ACCEPTED, 1=REJECTED, 2=INDETERMINATE. Logs a one-liner per attempt.
# Reads the current $MODEL global so the loop can swap models between calls.
probe() {
    local n="$1"
    local body resp code resp_body outcome attempt=0
    body=$(jq -nc --arg model "$MODEL" --argjson n "$n" \
        '{model:$model, max_tokens:$n, system:"",
          messages:[{role:"user", content:"Reply with the single character: 1"}]}')

    while :; do
        attempt=$((attempt + 1))
        # --write-out '\n%{http_code}' appends the code on its own line,
        # so multi-line JSON bodies stay intact.
        resp=$(curl --silent --show-error --max-time "$CURL_TIMEOUT" \
            --write-out $'\n%{http_code}' \
            -H "x-api-key: $KEY" \
            -H "anthropic-version: 2023-06-01" \
            -H "content-type: application/json" \
            -X POST "$URL" \
            -d "$body" 2>&1) || {
                log "$MODEL n=$n HTTP <network error> (attempt $attempt) INDETERMINATE"
                if [ "$attempt" -le "$INDETERMINATE_RETRIES" ]; then continue; fi
                return 2
            }
        code=$(printf '%s' "$resp" | tail -n1)
        resp_body=$(printf '%s' "$resp" | sed '$d')
        outcome=$(classify "$code" "$resp_body")
        case "$outcome" in
            ACCEPTED)
                log "$MODEL n=$n HTTP $code ACCEPTED"
                return 0 ;;
            REJECTED)
                local short
                short=$(printf '%s' "$resp_body" | tr -d '\n' | head -c 160)
                log "$MODEL n=$n HTTP $code REJECTED — body: $short"
                return 1 ;;
            *)
                local short
                short=$(printf '%s' "$resp_body" | tr -d '\n' | head -c 160)
                log "$MODEL n=$n HTTP $code INDETERMINATE (attempt $attempt) — body: $short"
                if [ "$attempt" -le "$INDETERMINATE_RETRIES" ]; then continue; fi
                return 2 ;;
        esac
    done
}

# probe_model <model_name>
# Echoes "<max_tokens> <phase2_rounds> <phase2_skipped>" on stdout and
# returns 0 on success, non-zero on failure (2=indeterminate, 4=cap above ceiling).
# Reads/writes the global $MODEL so probe() sends the right body.
probe_model() {
    local model="$1"
    MODEL="$model"

    local lo phase1_hi rc mid hi already_tested_lo_plus_1 skip_phase2 round

    sep "--- $MODEL ---"
    log "Phase 1: exponential search 2^$EXP_LO .. 2^$EXP_HI"
    lo=0
    phase1_hi=""
    for k in $(seq "$EXP_LO" "$EXP_HI"); do
        n=$((1 << k))
        # `|| rc=$?` instead of `; rc=$?` — under `set -e`, a `;`-separated
        # failing command aborts the shell before `rc=$?` ever runs. The
        # `||` form is the standard idiom: the assignment runs only when
        # probe returned non-zero, and `set -e` stays quiet because the
        # command is part of an `||` list. `rc=0` first because `||` skips
        # the assignment when probe succeeds, leaving `rc` unset under
        # `set -u`.
        rc=0; probe "$n" || rc=$?
        case "$rc" in
            0) lo=$n ;;
            1) phase1_hi=$n; break ;;
            *) err "$MODEL: Phase 1 indeterminate at n=$n (2^$k); skipping this model"
               echo "0 0 0"
               return 2 ;;
        esac
    done

    if [ -z "$phase1_hi" ]; then
        err "$MODEL: all probes accepted up to 2^$EXP_HI = $((1<<EXP_HI)); raise EXP_HI"
        echo "0 0 0"
        return 4
    fi

    log "Phase 1 result: lo=$lo (largest accepted), phase1_hi=$phase1_hi (smallest rejected)"

    # Phase 1.5 — cheap boundary probe. Most providers (including
    # MiniMax-M3) reject lo+1 outright — the boundary is sharp at the
    # largest accepted value. One HTTP call decides whether the entire
    # binary search is needed.
    log "Quick boundary probe: lo+1=$((lo+1))"
    rc=0; probe "$((lo+1))" || rc=$?
    already_tested_lo_plus_1=1   # we just probed the original lo+1
    skip_phase2=0
    case "$rc" in
        1)
            log "  lo+1 REJECTED -> sharp boundary at $lo; DONE"
            skip_phase2=1
            ;;
        0)
            log "  lo+1 ACCEPTED -> boundary > phase1_hi; raising lo and entering Phase 2"
            lo=$((lo+1))
            already_tested_lo_plus_1=0   # NEW lo+1 (old lo+2) is untested
            ;;
        *)
            err "$MODEL: lo+1 indeterminate -> entering Phase 2 anyway"
            already_tested_lo_plus_1=0
            ;;
    esac

    # Phase 2: divide y vencerás. Each round we test the midpoint of the
    # current band [lo, hi] and shrink the band by half:
    #   - mid ACCEPTED → the answer is in [mid, hi]; new lo = mid.
    #   - mid REJECTED → the answer is in [lo, mid]; new hi = mid.
    # We stop when hi - lo <= 1, meaning lo is the largest accepted and
    # lo+1 is the smallest rejected — that IS the exact boundary.
    round=0
    if [ "$skip_phase2" -eq 0 ]; then
        hi=$phase1_hi
        log "Phase 2: divide y vencerás on [$((lo+1)) .. $((hi-1))]"
        while [ $((hi - lo)) -gt 1 ]; do
            round=$((round + 1))
            mid=$(( (lo + hi) / 2 ))
            log "  round $round: range=[$lo .. $hi] (width=$((hi-lo))), test mid=$mid"
            rc=0; probe "$mid" || rc=$?
            case "$rc" in
                0)
                    lo=$mid
                    log "    mid=$mid ACCEPTED  -> range narrows to [$lo .. $hi]"
                    ;;
                1)
                    hi=$mid
                    log "    mid=$mid REJECTED  -> range narrows to [$lo .. $hi]"
                    if [ $((mid - lo)) -eq 1 ]; then
                        already_tested_lo_plus_1=1
                    fi
                    ;;
                *)
                    err "$MODEL: Phase 2 indeterminate at mid=$mid; skipping this model"
                    echo "0 0 0"
                    return 2
                    ;;
            esac
        done
    fi

    # Final verification: lo+1 must be rejected. Skip if the bisection
    # (or the quick boundary probe) already tested it.
    if [ "$already_tested_lo_plus_1" -eq 0 ]; then
        log "Verifying lo+1=$((lo+1)) is REJECTED..."
        rc=0; probe "$((lo+1))" || rc=$?
        if [ "$rc" -ne 1 ]; then
            err "$MODEL: lo+1=$((lo+1)) was NOT rejected (rc=$rc); algorithm drift?"
            echo "0 0 0"
            return 3
        fi
    fi

    log "$MODEL: max_tokens = $lo"
    echo "$lo $round $skip_phase2"
    return 0
}

# Banner
log "endpoint = $URL"
log "key len  = ${#KEY} (not echoed)"
log "range    = 2^$EXP_LO .. 2^$EXP_HI  (=$((1<<EXP_LO)) .. $((1<<EXP_HI)))"
log "models   = ${MODELS[*]}"

# Probe every model sequentially. Each call returns
# "<max_tokens> <rounds> <phase2_skipped>" on stdout; failures use rc>=2
# and emit a zero triple so the summary stays well-formed.
declare -a MAX_TOKENS=()
declare -a PHASE2_ROUNDS=()
declare -a PHASE2_SKIPPED=()
declare -a MODEL_RC=()

for model in "${MODELS[@]}"; do
    if out=$(probe_model "$model"); then
        rc=0
    else
        rc=$?
    fi
    # `read` ignores the trailing zero in the failure triple; we still
    # record a zero max_tokens so the summary line is unambiguous.
    read -r max_tokens rounds skipped <<< "$out"
    MAX_TOKENS+=("$max_tokens")
    PHASE2_ROUNDS+=("$rounds")
    PHASE2_SKIPPED+=("$skipped")
    MODEL_RC+=("$rc")
    echo
done

# Final summary — clean table, just model + max_tokens (no lo+1, no phase1_hi).
echo "================================================="
echo "SUMMARY — max_tokens per model"
echo "================================================="
printf '%-25s %-12s %s\n' "model" "max_tokens" "phase2"
printf '%-25s %-12s %s\n' "-----" "----------" "------"
ok_count=0
fail_count=0
for i in "${!MODELS[@]}"; do
    model="${MODELS[$i]}"
    mt="${MAX_TOKENS[$i]}"
    rounds="${PHASE2_ROUNDS[$i]}"
    skipped="${PHASE2_SKIPPED[$i]}"
    rc="${MODEL_RC[$i]}"
    if [ "$rc" -eq 0 ]; then
        if [ "$skipped" = "1" ]; then
            phase2_label="skipped"
        else
            phase2_label="ran (${rounds}r)"
        fi
        printf '%-25s %-12s %s\n' "$model" "$mt" "$phase2_label"
        ok_count=$((ok_count + 1))
    else
        printf '%-25s %-12s %s\n' "$model" "FAILED" "rc=$rc"
        fail_count=$((fail_count + 1))
    fi
done
echo "================================================="
log "ok=$ok_count  failed=$fail_count"

if [ "$fail_count" -gt 0 ]; then
    exit 5
fi