#!/usr/bin/env bash
# Fail if any site in src/ mutates the MOAGAN_NON_INTERACTIVE env var
# outside the sanctioned helper.
#
# Why: MOAGAN_NON_INTERACTIVE is process-global and is read inside
# RunContext::new / RunContext::new_with_config. Under `cargo test`'s
# thread-per-test parallelism, two tests mutating it concurrently observe
# each other's writes — a sibling's remove_var landing between a test's
# set_var and its RunContext construction yields an interactive context
# where the test asked for the opposite.
#
# That is not hypothetical: issue #791 tracked a ~1-in-8 failure of
# phases::phase::tests::run_context_honours_env_one caused by an
# unguarded set_var/remove_var pair in a phases::discover_summary test.
# The existing TEST_NON_INTERACTIVE_LOCK could not help, because a lock
# only serialises the writers that take it.
#
# The single sanctioned mutation point is
# crate::test_support::with_non_interactive (src/test_support.rs), which
# pairs the lock with a save-and-restore Drop guard. This guard makes the
# invariant mechanical instead of tribal.
#
# Scope: src/ only. Integration tests under tests/ set the variable via
# `cmd.env(...)` on a child process, which cannot race the parent's env.
set -euo pipefail

readonly ALLOWED_FILE="src/test_support.rs"
readonly VAR="MOAGAN_NON_INTERACTIVE"

hits=""

while IFS= read -r -d '' file; do
    if [[ "${file}" == "./${ALLOWED_FILE}" || "${file}" == "${ALLOWED_FILE}" ]]; then
        continue
    fi
    # Match set_var / remove_var calls naming the variable. The var name
    # and the call are on the same line at every current call site; a
    # multi-line spelling would slip past this grep, which is an accepted
    # limit of a line-based guard.
    file_hits=$(grep -nE "(set_var|remove_var)\([[:space:]]*\"${VAR}\"" "${file}" 2>/dev/null || true)
    if [[ -n "${file_hits}" ]]; then
        hits+="${file}"$'\n'
        hits+="${file_hits}"$'\n'
    fi
done < <(find src -name '*.rs' -print0)

if [[ -n "${hits}" ]]; then
    {
        echo "ERROR: ${VAR} mutated outside ${ALLOWED_FILE}:"
        echo ""
        printf "%s" "${hits}"
        echo ""
        echo "Use crate::test_support::with_non_interactive(Some(\"1\"), || { ... })"
        echo "or with_non_interactive(None, || { ... }) for the unset baseline."
        echo "It holds TEST_NON_INTERACTIVE_LOCK and restores the previous value"
        echo "on both the normal and the panic path."
        echo ""
        echo "See scripts/check-non-interactive-env-guard.sh and issue #791."
    } >&2
    exit 1
fi

echo "OK: ${VAR} only mutated via ${ALLOWED_FILE}"
