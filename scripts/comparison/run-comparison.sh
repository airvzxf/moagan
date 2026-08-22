#!/bin/bash
# Sequential launch of 6 real moagan discover tests to compare
# cardinality × temperature × replica configurations.
#
# Each test uses a fixed RUNS_DIR; if the dir already exists the test
# is skipped (so an interrupted/failed run can be re-launched after
# the user manually deletes the dir).
#
# After each test completes, the run dir is deleted so the next
# launch of this script starts fresh.
#
# Cardinalidad mínima = 80 (validated via mini-prueba). F2
# (Track G.2): the comparison still uses the legacy 4-dim ×
# 2-facet default layout (8 cells); the script now derives
# `--sketches-per-cell = ceil(cardinality / 8)` so the v0.5
# cardinality floor survives the rename.

set -u

REPO=/home/wolf/workspace/projects/moagan
PROMPT_FILE=/home/wolf/workspace/moagan/rcalculator/PROMPT.md
BASE_DIR=/home/wolf/workspace/moagan/rcalculator
# Use the debug binary (no SanCov). The coverage binary at
# target/coverage/moagan writes an unbounded .profraw file (66G over
# 5h 40min in run8) because every counter increments the file
# without a cap. We don't need coverage for the comparison test;
# we just need the sketches.
BINARY="$REPO/target/debug/moagan"
LOG_DIR=/tmp/moagan-comparison
mkdir -p "$LOG_DIR"

# Common environment
set -a
source "$REPO/.env"
set +a
export MOAGAN_QUIET=1
export MOAGAN_MINIMAX_ENDPOINT="https://api.minimax.io/anthropic/v1"
export PROMPT=$(cat "$PROMPT_FILE")
export RUST_LOG="info,moagan::discovery=trace,moagan::llm=trace,moagan::telemetry=trace"
export MOAGAN_TELEMETRY_FLUSH_EVERY=20

PARALLEL=64

# Track which runs-dir is currently active so the trap can clean
# up on signals (SIGTERM, SIGINT, Ctrl+C). Without this, a kill
# mid-run leaves the runs-dir on disk and the next script launch
# SKIPs it instead of re-running the test.
CURRENT_RUNS_DIR=""

cleanup_on_signal() {
    local sig="$1"
    if [[ -n "$CURRENT_RUNS_DIR" && -d "$CURRENT_RUNS_DIR" ]]; then
        echo "[$(date -Iseconds)] caught $sig; cleaning up $CURRENT_RUNS_DIR"
        rm -rf "$CURRENT_RUNS_DIR"
    fi
    exit 130
}
trap 'cleanup_on_signal SIGINT' INT
trap 'cleanup_on_signal SIGTERM' TERM

run_test() {
    local RUN_LABEL="$1"
    local CARD="$2"
    local TEMP_PROFILE="$3"
    local RUNS_DIR="$BASE_DIR/$RUN_LABEL"
    CURRENT_RUNS_DIR="$RUNS_DIR"

    if [[ -d "$RUNS_DIR" ]]; then
        echo "[$(date -Iseconds)] SKIP $RUN_LABEL (dir exists: $RUNS_DIR)"
        CURRENT_RUNS_DIR=""
        return 0
    fi

    mkdir -p "$RUNS_DIR"
    # No LLVM_PROFILE_FILE — the debug binary has no SanCov runtime,
    # so no .profraw is written. Keeps disk usage bounded.
    export RUNS_DIR

    local LOG="$LOG_DIR/${RUN_LABEL}.log"
    # F2 (Track G.2): `CARD` is the v0.5 `cardinality` floor
    # (cells × sketches_per_cell). With the legacy 4-dim ×
    # 2-facet default (`--dimensions 4 --facets-per-dimension 2`)
    # we have 8 cells, so `sketches_per_cell = CARD / 8`.
    # `ceil`-division so e.g. CARD=256 → 32 per cell.
    local SPC=$(( (CARD + 7) / 8 ))
    echo "[$(date -Iseconds)] START $RUN_LABEL cardinality=$CARD (sketches_per_cell=$SPC, cells=8) profile='$TEMP_PROFILE' parallel=$PARALLEL"
    echo "  log: $LOG"
    echo "  runs-dir: $RUNS_DIR"

    local T0
    T0=$(date +%s)
    "$BINARY" discover \
        --prompt "$PROMPT" \
        --provider minimax \
        --max-parallelism "$PARALLEL" \
        --non-interactive \
        --sketches-per-cell "$SPC" \
        --dimensions 4 \
        --facets-per-dimension 2 \
        --runs-dir "$RUNS_DIR" \
        --temperature-profile "$TEMP_PROFILE" \
        2>&1 | tee "$LOG"
    local RC=${PIPESTATUS[0]}
    local T1
    T1=$(date +%s)
    local ELAPSED=$((T1-T0))
    echo "[$(date -Iseconds)] END $RUN_LABEL rc=$RC elapsed=${ELAPSED}s"

    # PR-D2 cleanup hardening: always remove the runs-dir on exit,
    # not just on rc=0. On rc ≠ 0 the dir is moved to a
    # `.failed-<unix-ts>` sibling so the operator can inspect
    # the partial state without it blocking the next launch.
    # Verified context: run8 on 2026-08-19 failed with rc=80 and
    # left a 66 GB `.coverage-active/active.profraw` on disk
    # because the cleanup was conditional on rc=0.
    if [[ $RC -eq 0 ]]; then
        rm -rf "$RUNS_DIR"
        echo "[$(date -Iseconds)] CLEAN $RUN_LABEL (removed $RUNS_DIR)"
    else
        local quarantine="${RUNS_DIR}.failed-$(date +%s)"
        mv "$RUNS_DIR" "$quarantine"
        echo "[$(date -Iseconds)] FAILED $RUN_LABEL rc=$RC (moved to $quarantine)"
    fi
    CURRENT_RUNS_DIR=""
    return $RC
}

# test 1: cardinalidad 80, 21 temps, replicas=1 → 1680 llamadas
run_test "run8"  80 \
    'provider=MiniMax-M3;temperatures=0.0,0.1,0.2,0.3,0.4,0.5,1.0,1.1,1.2,1.3,1.4,1.5,1.6,1.7,1.8,1.9,2.0;replicas=1'

# test 2: cardinalidad 80, 1 temp, replicas=1 → 80 llamadas
run_test "run9"  80 \
    'provider=MiniMax-M3;temperatures=1.0;replicas=1'

# test 3: cardinalidad 80, 1 temp, replicas=21 → 1680 llamadas
run_test "run10" 80 \
    'provider=MiniMax-M3;temperatures=1.0;replicas=21'

# test 4: cardinalidad 256, 6 temps, replicas=1 → 1536 llamadas
run_test "run11" 256 \
    'provider=MiniMax-M3;temperatures=0.0,0.3,1.0,1.2,1.5,1.9;replicas=1'

# test 5: cardinalidad 1600, 1 temp, replicas=1 → 1600 llamadas
run_test "run12" 1600 \
    'provider=MiniMax-M3;temperatures=1.0;replicas=1'

# test 6: cardinalidad 256, 1 temp, replicas=6 → 1536 llamadas
run_test "run13" 256 \
    'provider=MiniMax-M3;temperatures=1.0;replicas=6'

echo "[$(date -Iseconds)] all tests done"
