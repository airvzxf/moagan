#!/usr/bin/env bash
# Smoke tests for `moagan discover`: outcome checks on the artefacts of
# real mock runs (no grep on source text).
#
# Each test runs the binary against tests/fixtures/mock_provider in a
# fresh MOAGAN_HOME and asserts on what the run leaves on disk. Prints
# `OK: <test_name>` per passing test.
#
# Usage:  ./scripts/smoke_discovery.sh
# Exit:   0 when all tests pass, 1 otherwise.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${BIN:-${ROOT}/target/debug/moagan}"
MOCK="${ROOT}/tests/fixtures/mock_provider"
PASS=0
FAIL=0
FAILED_TESTS=()

if [[ ! -x "$BIN" ]]; then
  echo "moagan binary not built at $BIN; run 'cargo build' first"
  exit 1
fi

run_test() {
  local name="$1"
  local body="$2"
  local out
  out="$(mktemp)"
  bash -c "$body" >"$out" 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' "$out"
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
  rm -f "$out"
}

# Run one mock discover into a fresh home; echo the run dir.
mock_run() {
  local home
  home="$(mktemp -d "${TMPDIR:-/tmp}/moagan-smoke-discover.XXXXXX")"
  MOAGAN_HOME="$home" "$BIN" discover --non-interactive \
    --prompt "Enumera los 7 colores del arcoiris en orden" \
    --provider mock:mock-model --mock-dir "$MOCK" "$@" \
    --runs-dir "$home" >"$home/stdout.log" 2>"$home/stderr.log"
  echo "$?" >"$home/rc"
  echo "$home"
}

# ---------------------------------------------------------------------
# Run A: fixed 2-cell matrix, 3 sketches per cell (6 sketches).
# ---------------------------------------------------------------------
HOME_A="$(mock_run --matrix-spec "auth=oauth,api-key" --sketches-per-cell 3)"
RUN_A="$(ls -d "$HOME_A"/.runs/*/ 2>/dev/null | head -1)"
export HOME_A RUN_A

run_test "discover_exits_zero" '[[ "$(cat "$HOME_A/rc")" == 0 ]] || { cat "$HOME_A/stderr.log" | tail -20; exit 1; }'
run_test "discover_writes_six_canonical_sketches" \
  '[[ $(ls "$RUN_A/sketches" | grep -cE "^sk_[0-9]{4,}\.json$") == 6 ]]'
run_test "discover_persists_the_verbatim_prompt" \
  '[[ "$(cat "$RUN_A/prompt.md")" == "Enumera los 7 colores del arcoiris en orden" ]]'
run_test "catalogue_readme_reports_full_coverage" \
  'grep -q "^- Theses in this catalogue: 6 of 6 sketches (100.0 %)$" "$RUN_A/final/README.md"'
run_test "catalogue_has_one_facet_file_per_cell" \
  '[[ -f "$RUN_A/final/auth/oauth.md" && -f "$RUN_A/final/auth/api-key.md" ]]'
run_test "catalog_json_lists_every_sketch_once" \
  'python3 -c "import json,sys; c=json.load(open(sys.argv[1])); ids=[s[\"id\"] for s in c[\"sketches\"]]; sys.exit(0 if len(ids)==6 and len(set(ids))==6 and c[\"schema\"]==\"discover-catalog-v1\" else 1)" "$RUN_A/final/catalog.json"'
run_test "every_sketch_heads_exactly_one_facet_entry" \
  'for f in "$RUN_A"/sketches/sk_*.json; do case "$f" in *.meta.json) continue;; esac; id=$(basename "$f" .json); n=$(cat "$RUN_A"/final/auth/*.md | grep -cx "### $id"); [[ $n == 1 ]] || { echo "$id: $n"; exit 1; }; done'
run_test "catalogue_has_a_constraint_annex" \
  'grep -q "^# Constraint annex$" "$RUN_A/final/constraints-annex.md"'
run_test "no_legacy_category_or_summary_files" \
  '! ls "$RUN_A/final" | grep -qE "^(cat_|summary\.)"'
run_test "no_llm_call_after_the_sketches" \
  'gzip -dc "$RUN_A/telemetry/calls.jsonl.gz" | python3 -c "import json,sys; roles={json.loads(l)[\"role\"] for l in sys.stdin if l.strip()}; sys.exit(0 if roles <= {\"intake\", \"sketch\"} else 1)"'

# ---------------------------------------------------------------------
# Run B: deprecated flags are accepted, warn, and change nothing.
# ---------------------------------------------------------------------
HOME_B="$(mock_run --matrix-spec "auth=oauth" --sketches-per-cell 1 --cluster-threshold 0.5 --cache-facets)"
RUN_B="$(ls -d "$HOME_B"/.runs/*/ 2>/dev/null | head -1)"
export HOME_B RUN_B

run_test "deprecated_flags_are_accepted" '[[ "$(cat "$HOME_B/rc")" == 0 ]]'
run_test "deprecated_flags_warn_once_each" \
  '[[ $(cat "$HOME_B/stdout.log" "$HOME_B/stderr.log" | grep -c "cluster-threshold has no effect") -ge 1 && $(cat "$HOME_B/stdout.log" "$HOME_B/stderr.log" | grep -c "cache-facets has no effect") -ge 1 ]]'
run_test "deprecated_flags_are_hidden_from_help" \
  '! "$BIN" discover --help | grep -qE "cluster-threshold|cache-facets"'
run_test "single_cell_run_catalogues_its_sketch" \
  'grep -q "^- Theses in this catalogue: 1 of 1 sketches (100.0 %)$" "$RUN_B/final/README.md"'

for d in "$HOME_A" "$HOME_B"; do
  case "$d" in */moagan-smoke-discover.*) rm -rf -- "$d" ;; esac
done

echo ""
echo "smoke_discovery: ${PASS} passed, ${FAIL} failed"
if [[ $FAIL -gt 0 ]]; then
  printf '  failed: %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi
exit 0
