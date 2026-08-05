#!/usr/bin/env bash
# Smoke gate for `moagan validate <brief_path>` (D.14.4).
#
# Six black-box checks against a freshly built debug binary:
#   1. cargo build succeeds.
#   2. `moagan validate --help` exits 0 and mentions BRIEF_PATH.
#   3. Clean brief  → exit 0.
#   4. Brief with a forbidden tech → exit 1, stderr mentions "hard:".
#   5. Missing brief path → exit 2.
#   6. Malformed JSON     → exit 2.
#
# Run from the repo root after `cargo build`.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT}/target/debug/moagan"
PASS=0
FAIL=0
FAILED=()
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

# run_check <name> <expected_rc> <command...>
run_check() {
  local name="${1}"
  local expected_rc="${2}"
  shift 2
  local rc=0
  "$@" >"${TMP}/out" 2>"${TMP}/err" || rc=$?
  if [[ "${rc}" -eq "${expected_rc}" ]]; then
    printf 'OK: %s\n' "${name}"
    PASS=$((PASS + 1))
  else
    printf 'FAIL: %s (rc=%d, expected %d)\n' "${name}" "${rc}" "${expected_rc}"
    printf '  stdout: '
    sed 's/^/    /' "${TMP}/out"
    printf '  stderr: '
    sed 's/^/    /' "${TMP}/err"
    FAIL=$((FAIL + 1))
    FAILED+=("${name}")
  fi
}

# run_check_grep <name> <expected_rc> <needle> <file>
run_check_grep() {
  local name="${1}"
  local expected_rc="${2}"
  local needle="${3}"
  local file="${4}"
  if [[ "${expected_rc}" -eq 0 ]] && grep -qF -- "${needle}" "${file}"; then
    printf 'OK: %s\n' "${name}"
    PASS=$((PASS + 1))
  elif [[ "${expected_rc}" -ne 0 ]]; then
    printf 'FAIL: %s (expected_rc=%d but used as success)\n' "${name}" "${expected_rc}"
    FAIL=$((FAIL + 1))
    FAILED+=("${name}")
  else
    printf 'FAIL: %s (needle %q not found in %s)\n' "${name}" "${needle}" "${file}"
    printf '  content: '
    sed 's/^/    /' "${file}"
    FAIL=$((FAIL + 1))
    FAILED+=("${name}")
  fi
}

# 1. cargo build
run_check 'cargo build' 0 \
  bash -c "cargo build --manifest-path '${ROOT}/Cargo.toml' >/dev/null"

# Pre-condition: binary must exist for the rest of the checks.
if [[ ! -x "${BIN}" ]]; then
  printf '\nPhase D validate smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
  printf '  - cargo build\n'
  exit 1
fi

# 2. validate --help exits 0 and surfaces BRIEF_PATH so operators
#    see the positional argument name.
"${BIN}" validate --help >"${TMP}/help" 2>&1
rc=$?
if [[ "${rc}" -eq 0 ]]; then
  printf 'OK: validate --help exits 0\n'
  PASS=$((PASS + 1))
else
  printf 'FAIL: validate --help (rc=%d)\n' "${rc}"
  FAIL=$((FAIL + 1))
  FAILED+=('validate --help exit 0')
fi
run_check_grep '--help mentions BRIEF_PATH' 0 'BRIEF_PATH' "${TMP}/help"

# 3. clean brief → exit 0
CLEAN="${TMP}/clean_brief.json"
cat >"${CLEAN}" <<'JSON'
{
  "problem": "Use the standard ROYGBIV order for the rainbow",
  "objectives": ["produce the rainbow"],
  "deliverables": ["ordered color list"],
  "constraints": [],
  "assumptions": [],
  "non_goals": [],
  "acceptance": ["seven distinct colors"],
  "risks": []
}
JSON
run_check 'clean brief exits 0' 0 "${BIN}" validate "${CLEAN}"

# 4. brief with a hard issue → exit 1 + stderr mentions "hard:"
HARD="${TMP}/hard_brief.json"
cat >"${HARD}" <<'JSON'
{
  "problem": "Use postgres for storage",
  "objectives": [],
  "deliverables": [],
  "constraints": [],
  "assumptions": [],
  "non_goals": [],
  "acceptance": [],
  "risks": []
}
JSON
run_check 'hard-issue brief exits 1' 1 \
  env MOAGAN_GATE_FORBIDDEN_TECHS=postgres "${BIN}" validate "${HARD}"
run_check_grep 'hard-issue stderr mentions "hard:"' 0 'hard:' "${TMP}/err"

# 5. missing brief → exit 2
MISSING="${TMP}/does_not_exist_brief.json"
run_check 'missing brief exits 2' 2 "${BIN}" validate "${MISSING}"

# 6. malformed JSON → exit 2
MALFORMED="${TMP}/malformed_brief.json"
printf '{ this is not json' >"${MALFORMED}"
run_check 'malformed JSON exits 2' 2 "${BIN}" validate "${MALFORMED}"

printf '\nPhase D validate smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if [[ ${FAIL} -gt 0 ]]; then
  printf 'FAILED:\n'
  printf '  - %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0