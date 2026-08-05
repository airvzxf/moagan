#!/usr/bin/env bash
# Smoke gate for `moagan diff <run_a> <run_b>` (D.14.2).
#
# Five black-box checks against a freshly built debug binary:
#   1. cargo build succeeds.
#   2. `moagan diff --help` exits 0 and surfaces RUN_A so operators
#      see the positional argument name.
#   3. Malformed run id → exit 2.
#   4. `--format json` is a recognised `--help` flag.
#   5. Self-diff (same id twice) → exit 2 with a clear message.
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
  printf '\nPhase D diff smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
  printf '  - cargo build\n'
  exit 1
fi

# 2. diff --help exits 0 and surfaces RUN_A so operators
#    see the positional argument name.
"${BIN}" diff --help >"${TMP}/help" 2>&1
rc=$?
if [[ "${rc}" -eq 0 ]]; then
  printf 'OK: diff --help exits 0\n'
  PASS=$((PASS + 1))
else
  printf 'FAIL: diff --help (rc=%d)\n' "${rc}"
  FAIL=$((FAIL + 1))
  FAILED+=('diff --help exit 0')
fi
run_check_grep '--help mentions RUN_A' 0 'RUN_A' "${TMP}/help"
run_check_grep '--help mentions RUN_B' 0 'RUN_B' "${TMP}/help"

# 3. malformed run id → exit 2
run_check 'malformed run id exits 2' 2 \
  "${BIN}" diff not-a-uuid also-not-a-uuid

# 4. --format json surfaces as a documented option in --help.
run_check_grep '--help mentions --format' 0 '--format' "${TMP}/help"
run_check_grep '--help mentions json value' 0 'json' "${TMP}/help"

# 5. self-diff (same id twice) → exit 2 + stderr mentions "cannot diff"
#    Use a parseable-but-unregistered UUID so the self-diff branch is
#    the one that fires first. The path /tmp/diff-home is used as a
#    scratch MOAGAN_HOME so the binary cannot accidentally look up
#    real runs.
run_check 'self-diff exits 2' 2 \
  env MOAGAN_HOME="${TMP}/diff-home" \
  "${BIN}" diff 01900000-0000-0000-0000-000000000000 01900000-0000-0000-0000-000000000000
run_check_grep 'self-diff stderr mentions "cannot diff"' 0 'cannot diff' "${TMP}/err"

printf '\nPhase D diff smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if [[ ${FAIL} -gt 0 ]]; then
  printf 'FAILED:\n'
  printf '  - %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0
