#!/usr/bin/env bash
# Guard: the GitHub Actions cache policy of ADR-0014.
#
#   "Only main writes the cache, one job per key writes it, and every
#    other run only reads it."
#
# Before ADR-0014 the cache layout had three silent defects, found on
# 2026-10-14 from the run logs:
#   1. `lookup-only: true` on pull requests skipped the DOWNLOAD (that
#      is what the input means) while `save-if` stayed true, so PRs
#      built cold and still saved ~350 MB entries under
#      `refs/pull/N/merge` that no other run can read.
#   2. Eight parallel ci.yml jobs shared one key; the first to finish
#      (`fmt-check`, which compiles nothing) won the save, so main's
#      entry held the registry only (87 MB) and every job recompiled
#      all 263 dependencies.
#   3. Jobs without a `shared-key` collided on Swatinem's job-id key
#      (three workflows each have a job called `sync`).
#
# Rules checked here (each failure prints one `ERROR:` line):
#   R1  .github/actions/rust-setup/action.yml saves only on main and
#       only when the caller asks (`save-if` names both
#       `inputs.save-cache == 'true'` and
#       `github.ref == 'refs/heads/main'`), sets
#       `cache-on-failure: false`, and does not use `lookup-only` or
#       `cache-workspace-crates`.
#   R2  No other workflow or action uses `Swatinem/rust-cache` or
#       `actions/cache` directly.
#   R3  Every `uses: ./.github/actions/rust-setup` step passes a
#       non-empty `shared-key`.
#   R4  Every shared key has exactly one step with
#       `save-cache: 'true'` (its single writer).
#   R5  No workflow or action sets `CARGO_INCREMENTAL`.
#   R6  release.yml does not use rust-setup (cold by design, AGENTS.md
#       §"Release is cold-by-design").
#
# Wired into `make guard-deps` (T0). Mirrors the structure of
# scripts/check-no-cancel-in-progress.sh.
set -euo pipefail

GITHUB_DIR="${GITHUB_DIR:-.github}"
ACTION="${GITHUB_DIR}/actions/rust-setup/action.yml"
WORKFLOWS_DIR="${GITHUB_DIR}/workflows"

errors=0
fail() {
    echo "ERROR: $*" >&2
    errors=$((errors + 1))
}

if [[ ! -f "${ACTION}" || ! -d "${WORKFLOWS_DIR}" ]]; then
    echo "ERROR: ${ACTION} or ${WORKFLOWS_DIR} not found" >&2
    exit 1
fi

# --- R1 ---------------------------------------------------------------
save_if="$(grep -E '^\s*save-if:' "${ACTION}" || true)"
if [[ -z "${save_if}" ]]; then
    fail "R1 ${ACTION}: no save-if (the cache must be saved only on main)"
else
    [[ "${save_if}" == *"inputs.save-cache == 'true'"* ]] \
        || fail "R1 ${ACTION}: save-if does not check inputs.save-cache == 'true'"
    [[ "${save_if}" == *"github.ref == 'refs/heads/main'"* ]] \
        || fail "R1 ${ACTION}: save-if does not check github.ref == 'refs/heads/main'"
fi
grep -qE '^\s*cache-on-failure:\s*false\s*$' "${ACTION}" \
    || fail "R1 ${ACTION}: cache-on-failure must be false (a failed build must not freeze a partial entry)"
if grep -qE '^\s*lookup-only:' "${ACTION}"; then
    fail "R1 ${ACTION}: lookup-only skips the download; remove it"
fi
if grep -qE '^\s*cache-workspace-crates:' "${ACTION}"; then
    fail "R1 ${ACTION}: cache-workspace-crates caches a stale moagan build; remove it"
fi

# --- R2 ---------------------------------------------------------------
while IFS= read -r hit; do
    [[ -z "${hit}" ]] && continue
    fail "R2 ${hit%%:*}: uses the cache directly; go through .github/actions/rust-setup"
done < <(grep -rlE '^\s*(-\s+)?uses:\s*(Swatinem/rust-cache|actions/cache)@' \
    "${WORKFLOWS_DIR}" "${GITHUB_DIR}/actions" | grep -vxF "${ACTION}" || true)

# --- R3 + R4 ----------------------------------------------------------
# One line per rust-setup step: `file<TAB>line<TAB>shared-key<TAB>save-cache`.
# A step is the `- ` item that holds the `uses:` line; its inputs are the
# following lines indented deeper than that item's dash.
steps="$(awk -v q="'" '
    function flush() {
        if (in_step && is_setup) printf "%s\t%d\t%s\t%s\n", file, at, key, save
        in_step = 0; is_setup = 0; key = ""; save = ""
    }
    function value(line) {
        sub(/^[^:]*:[[:space:]]*/, "", line)
        sub(/[[:space:]]+#.*$/, "", line)
        gsub(/"/, "", line)
        gsub(q, "", line)
        sub(/[[:space:]]+$/, "", line)
        return line
    }
    FNR == 1 { flush() }
    /^[[:space:]]*$/ || /^[[:space:]]*#/ { next }
    {
        indent = match($0, /[^ ]/) - 1
        if (in_step && indent <= dash) flush()
        if ($0 ~ /^[[:space:]]*-[[:space:]]/) {
            flush()
            in_step = 1; dash = indent; at = FNR; file = FILENAME
        }
        if (!in_step) next
        if ($0 ~ /uses:[[:space:]]*\.\/\.github\/actions\/rust-setup[[:space:]]*$/) is_setup = 1
        if ($0 ~ /^[[:space:]]*shared-key:/) key = value($0)
        if ($0 ~ /^[[:space:]]*save-cache:/) save = value($0)
    }
    END { flush() }
' "${WORKFLOWS_DIR}"/*.yml)"

callers=0
declare -A writers=()
while IFS=$'\t' read -r file line key save; do
    [[ -z "${file}" ]] && continue
    callers=$((callers + 1))
    if [[ -z "${key}" ]]; then
        fail "R3 ${file}:${line}: rust-setup step has no shared-key"
        continue
    fi
    : "${writers[${key}]:=0}"
    case "${save}" in
        true) writers[${key}]=$((writers[${key}] + 1)) ;;
        '' | false) ;;
        *) fail "R4 ${file}:${line}: save-cache must be 'true' or absent, got '${save}'" ;;
    esac
done <<< "${steps}"

for key in $(printf '%s\n' "${!writers[@]}" | sort); do
    count="${writers[${key}]}"
    if [[ "${count}" -ne 1 ]]; then
        fail "R4 shared-key ${key}: ${count} writers (save-cache: 'true'), want exactly 1"
    fi
done

# --- R5 ---------------------------------------------------------------
while IFS= read -r hit; do
    [[ -z "${hit}" ]] && continue
    fail "R5 ${hit}: sets CARGO_INCREMENTAL"
done < <(grep -rnE '^\s*CARGO_INCREMENTAL:' "${WORKFLOWS_DIR}" "${GITHUB_DIR}/actions" \
    | cut -d: -f1,2 || true)

# --- R6 ---------------------------------------------------------------
if grep -qE 'uses:\s*\./\.github/actions/rust-setup' "${WORKFLOWS_DIR}/release.yml"; then
    fail "R6 ${WORKFLOWS_DIR}/release.yml: uses rust-setup (release is cold by design)"
fi

if [[ "${errors}" -ne 0 ]]; then
    echo "ERROR: ${errors} cache-policy violation(s). See docs/adr/0014-ci-cache-main-writes.md." >&2
    exit 1
fi
echo "OK: CI cache policy holds (${callers} rust-setup steps, ${#writers[@]} shared keys, one writer each)"
