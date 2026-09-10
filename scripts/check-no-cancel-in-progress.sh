#!/usr/bin/env bash
# Fail if any workflow in .github/workflows/ sets `cancel-in-progress: true`.
#
# Rationale (see docs/adr/0011-no-cancel-in-progress.md):
#   1. A cancel 14 min into a token-billed LLM run wasted upstream spend
#      AND marked the previous commit as red on the commit page
#      (closes #738).
#   2. A duplicate dispatch of `release.yml` raced the SBOM-upload step
#      because the default `queue: single` cancels pending runs
#      (closes #741).
#   3. The same regression class re-surfaced when EPIC #852 introduced
#      three doc-sync workflows with `cancel-in-progress: true` and
#      didn't update the misleading "ONLY workflow" comment in ci.yml
#      (closes #878).
#
# This script is the load-bearing control. It mirrors the structure of
# scripts/check-no-forbidden-crates.sh (ADR-0001's enforcement sibling).
# A future PR that wants to re-enable `cancel-in-progress: true` must:
#   1. amend docs/adr/0011-no-cancel-in-progress.md with a new
#      Re-evaluation section justifying the change,
#   2. update this script's allowlist (currently empty),
#   3. both in the same PR.
#
# Without this guard, the policy in ADR-0011 lives only in prose and
# can drift the same way the `ci.yml:10-23` comment drifted.
set -euo pipefail

WORKFLOWS_DIR="${WORKFLOWS_DIR:-.github/workflows}"

# Workflows that are explicitly allowed to set `cancel-in-progress: true`.
# The list is empty by default; populating it requires amending ADR-0011.
allowlist=()

if [[ ! -d "${WORKFLOWS_DIR}" ]]; then
    echo "ERROR: ${WORKFLOWS_DIR} not found" >&2
    exit 1
fi

errors=0
matched_files=()

while IFS= read -r line; do
    # `line` is `path:linenum: cancel-in-progress: true` from grep -nE.
    file="${line%%:*}"
    rest="${line#*:}"
    linenum="${rest%%:*}"

    # Skip allow-listed files (currently none).
    skip=0
    for allowed in "${allowlist[@]}"; do
        if [[ "${file}" == "${allowed}" ]]; then
            skip=1
            break
        fi
    done
    if [[ "${skip}" -eq 1 ]]; then
        continue
    fi

    echo "ERROR: cancel-in-progress: true detected in ${file}:${linenum} (forbidden by ADR-0011)" >&2
    matched_files+=("${file}")
    errors=$((errors + 1))
done < <(grep -rHnE '^\s*cancel-in-progress:\s*true(\s*|\s*#)' "${WORKFLOWS_DIR}"/*.yml || true)

if [[ "${errors}" -ne 0 ]]; then
    echo "ERROR: ${errors} cancel-in-progress: true violation(s) detected across ${#matched_files[@]} workflow file(s)." >&2
    echo "       See docs/adr/0011-no-cancel-in-progress.md for the policy." >&2
    echo "       Removing the flag requires amending ADR-0011 with a Re-evaluation entry." >&2
    exit 1
fi

echo "OK: no cancel-in-progress: true in ${WORKFLOWS_DIR}"
