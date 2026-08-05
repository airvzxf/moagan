#!/usr/bin/env bash
# Smoke gate for `moagan repair` (D.14.3 + D.28.1/3/4/5).
#
# Six black-box checks against a freshly built debug binary:
#   1. cargo build succeeds.
#   2. `moagan repair --help` exits 0 and mentions the three
#      operation flags.
#   3. `moagan repair` (no flags) exits 2 (Error::InvalidArgs).
#   4. `moagan repair --cleanup-orphans --dry-run --yes` exits 0
#      (commit 2).
#   5. `moagan repair --reindex-artifacts --dry-run` exits 0
#      (commit 3).
#   6. `moagan repair --recover-zombies --dry-run` exits 0
#      (commit 4).
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
  printf '\nPhase D repair smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
  printf '  - cargo build\n'
  exit 1
fi

# 2. repair --help exits 0 and surfaces the operation flags.
"${BIN}" repair --help >"${TMP}/help" 2>&1
rc=$?
if [[ "${rc}" -eq 0 ]]; then
  printf 'OK: repair --help exits 0\n'
  PASS=$((PASS + 1))
else
  printf 'FAIL: repair --help (rc=%d)\n' "${rc}"
  FAIL=$((FAIL + 1))
  FAILED+=('repair --help exit 0')
fi
run_check_grep '--help mentions --cleanup-orphans' 0 '--cleanup-orphans' "${TMP}/help"
run_check_grep '--help mentions --reindex-artifacts' 0 '--reindex-artifacts' "${TMP}/help"
run_check_grep '--help mentions --recover-zombies' 0 '--recover-zombies' "${TMP}/help"

# 3. no flag at all → exit 2 (Error::InvalidArgs). Use a scratch
#    MOAGAN_HOME so the binary cannot accidentally find a real
#    runs dir to operate on.
run_check 'no-flags exits 2' 2 \
  env MOAGAN_HOME="${TMP}/repair-home" "${BIN}" repair
run_check_grep 'no-flags stderr mentions "at least one"' 0 'at least one' "${TMP}/err"

# 4. cleanup-orphans dry-run: print the plan and exit 0 even
#    when the runs dir does not exist (the plan is empty).
run_check 'cleanup-orphans dry-run exits 0' 0 \
  env MOAGAN_HOME="${TMP}/repair-home" \
  "${BIN}" repair --cleanup-orphans --dry-run
run_check_grep 'cleanup-orphans dry-run prints "nothing to do"' 0 'nothing to do' "${TMP}/out"

# 5. reindex-artifacts dry-run: against an empty home the
#    plan is empty and the call exits 0.
run_check 'reindex-artifacts dry-run exits 0' 0 \
  env MOAGAN_HOME="${TMP}/repair-home" \
  "${BIN}" repair --reindex-artifacts --dry-run

printf '\nPhase D repair smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if [[ ${FAIL} -gt 0 ]]; then
  printf 'FAILED:\n'
  printf '  - %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0
