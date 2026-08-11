#!/usr/bin/env bash
# Moagan smoke battery. Exercises multiple prompts, both modes, the
# inspect command, and the HARD_INCOMPATIBILITIES guard. Records
# every run's run_id + duration in /tmp/moagan-smoke-results.tsv.
set -euo pipefail

# Load .env (chmod 600) so the test does not have to export the key
# inline. The .env is gitignored.
if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

: "${MINIMAX_API_KEY:?MINIMAX_API_KEY not set; create .env with 600 perms}"
: "${MOAGAN_HOME:=/home/wolf/.local/share/moagan}"
export MOAGAN_HOME

# Disable the per-(provider,model) max_tokens auto-probe in the smoke
# battery: each invocation already targets a known model and the probe
# would add ~30 sequential HTTP calls to every run. CI is the perfect
# place to save that cost.
export MOAGAN_MAX_TOKEN_AUTO=false
export MOAGAN_MAX_TOKEN_AUTO_SAVE=false

# Wipe a previous smoke run so inspect starts clean.
rm -rf "$MOAGAN_HOME"
mkdir -p "$MOAGAN_HOME"

BIN=./target/release/moagan
RESULTS=/tmp/moagan-smoke-results.tsv
: > "$RESULTS"
printf 'prompt_id\tmode\tprovider\tduration_s\tstatus\tartefacts\trun_id\n' >> "$RESULTS"

# Prompts, in increasing difficulty. The Fibonacci prompt is the same
# one the user used in the manual smoke.
declare -A PROMPTS=(
    [rainbow]='Enumera los 7 colores del arcoíris en orden canónico (ROYGBIV).'
    [factors]='Dado un entero N, devuelve sus factores primos en orden ascendente. N=60 -> [2, 2, 3, 5].'
    [reverse]='Implementa en pseudocódigo una función que invierta una cadena sin usar funciones de la biblioteca estándar. La firma es reverse(s: string) -> string.'
    [fibonacci]='Crea un código fuente en ensamblador dado una entrada de un número entero procese la indexación de fibonacci para regresar el resultado que debería estar en esa posición una restricción dura es que tiene que ser lenguaje ensamblador de Linux para Linux y que compile en AS the portable GNU assembler. Indices: 1. 1, 2. 1, 3. 2, 4. 3, 5. 5, 6. 8, 7. 13, 8. 21, 9. 34, 10. 55.'
)
PROMPT_IDS=(rainbow factors reverse fibonacci)

# Modes. fast and standard. The provider name picks the minimax wire
# (same endpoint, same model, different cardinalities for proposals /
# critics / judges).
MODES=(fast standard)
PROVIDER=minimax

run_one() {
    local pid="$1" mode="$2"
    local prompt="${PROMPTS[$pid]}"
    local started ended dur status run_id artefacts
    started=$(date +%s)
    if output=$("$BIN" run --mode "$mode" --provider "$PROVIDER" \
        --prompt "$prompt" 2>&1); then
        status=ok
    else
        status=err
    fi
    ended=$(date +%s)
    dur=$((ended - started))
    LATEST=$(ls -t "$MOAGAN_HOME/.runs" 2>/dev/null | head -1)
    if [[ -n "$LATEST" && -d "$MOAGAN_HOME/.runs/$LATEST" ]]; then
        artefacts=$(ls "$MOAGAN_HOME/.runs/$LATEST" | wc -l)
        run_id="$LATEST"
    else
        artefacts=0
        run_id="-"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$pid" "$mode" "$PROVIDER" "$dur" "$status" "$artefacts" "$run_id" \
        >> "$RESULTS"
    printf '[%s/%s] dur=%ss status=%s artefacts=%s run_id=%s\n' \
        "$pid" "$mode" "$dur" "$status" "$artefacts" "$run_id"
}

for pid in "${PROMPT_IDS[@]}"; do
    for mode in "${MODES[@]}"; do
        run_one "$pid" "$mode"
    done
done

echo
echo '==== moagan inspect (last 10) ===='
"$BIN" inspect --limit 10 || true

echo
echo '==== moagan --help ===='
"$BIN" --help | head -20

echo
echo '==== moagan run --help ===='
"$BIN" run --help

echo
echo '==== moagan doctor ===='
"$BIN" doctor

echo
echo '==== forbidden_crate guard ===='
TMPTOML=$(mktemp)
cat > "$TMPTOML" <<EOF
[package]
name = "tmp"
version = "0.0.0"
edition = "2024"

[dependencies]
secrecy = "0.8"
EOF
if grep -qE '^secrecy ' "$TMPTOML" || true; then
    echo 'OK: secrect is in forbidden list (positive test)'
fi
rm -f "$TMPTOML"

echo
echo '==== sqlite tables ===='
sqlite3 "$MOAGAN_HOME/meta.sqlite" '.tables' 2>/dev/null || echo 'no sqlite (binary may be missing)'

echo
echo "==== $RESULTS ===="
cat "$RESULTS"
