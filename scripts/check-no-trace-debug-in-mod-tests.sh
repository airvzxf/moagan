#!/usr/bin/env bash
# Fail if any `tracing::debug!` / `tracing::trace!` macro call lives inside
# an inline `#[cfg(test)] mod tests { ... }` block in src/. Tests that
# need to assert on DEBUG/TRACE events belong in a dedicated
# tests/integration_*_tracing.rs binary (one #[test] per binary, single-
# process isolation).
#
# Why: tracing_subscriber::fmt::try_init() at src/sandbox/process.rs:2535
# sets the process-global LevelFilter to ERROR when RUST_LOG is unset
# (verified against tracing-subscriber-0.3.23 EnvFilter::from_default_env
# which defaults the directive to LevelFilter::ERROR). Every
# tracing::debug! / tracing::trace! callsite that fires once in that
# process permanently marks itself as Interest::Never in tracing-core's
# callsite cache; tracing::subscriber::with_default (thread-local
# dispatcher override) cannot rescue it. The fix is per-binary isolation,
# enforced here mechanically.
#
# Background: §2.2 flake closed by PR #647 / #648 in v0.12.11; follow-up
# ticket #668 (https://github.com/airvzxf/moagan/issues/668).
#
# Heuristic: awk-based brace tracker. Handles line comments (// ...);
# block comments (/* ... */) are not stripped, so a `}` inside a block
# comment could cause a false-positive (we exit mod tests too early and
# then over-flag). In practice the moagan test modules do not contain
# such cases; if a future module does, switch to the python3 fallback
# noted in the plan.
set -euo pipefail

ROOTS=("src")

errors=0
hits_output=""

for root in "${ROOTS[@]}"; do
    [[ -d "${root}" ]] || continue
    while IFS= read -r -d '' file; do
        # Fast skip: if the file has neither the disallowed macros nor a
        # mod tests block, skip awk entirely.
        if ! grep -qE 'tracing::(debug|trace)!\(' "${file}" 2>/dev/null; then
            continue
        fi
        # Run the brace-aware scan.
        hits=$(awk '
            BEGIN { in_tests = 0; depth = 0; base_depth = -1 }
            {
                line = $0
                # Strip // line comments before counting braces.
                stripped = line
                sub(/\/\/.*/, "", stripped)
                # Detect `mod tests {` (with optional whitespace and
                # any preceding text like #[cfg(test)]).
                if (in_tests == 0 && stripped ~ /mod[[:space:]]+tests[[:space:]]*\{/) {
                    in_tests = 1
                    base_depth = depth
                }
                # Count braces in the comment-stripped line.
                n_open = gsub(/\{/, "&", stripped)
                n_close = gsub(/\}/, "&", stripped)
                depth += n_open - n_close
                # Exit mod tests when we return to the pre-mod depth.
                if (in_tests == 1 && depth <= base_depth) {
                    in_tests = 0
                    base_depth = -1
                }
                # Flag disallowed macros inside mod tests.
                if (in_tests == 1 && stripped ~ /tracing::(debug|trace)!/) {
                    printf "%s:%d: %s\n", FILENAME, FNR, $0
                }
            }
        ' "${file}" || true)
        if [[ -n "${hits}" ]]; then
            errors=$((errors + 1))
            hits_output+="ERROR: tracing::debug!/trace! inside #[cfg(test)] mod tests in ${file}"$'\n'
            hits_output+="${hits}"$'\n'
            hits_output+=""$'\n'
            hits_output+="Move this test to a dedicated tests/integration_*_tracing.rs binary,"$'\n'
            hits_output+="or refactor the assertion to tracing::info! / warn! / error!."$'\n'
            hits_output+="See scripts/check-no-trace-debug-in-mod-tests.sh for rationale."$'\n'
            hits_output+=""$'\n'
        fi
    done < <(find "${root}" -name '*.rs' -print0)
done

if [[ "${errors}" -ne 0 ]]; then
    printf "%s" "${hits_output}" >&2
    echo "ERROR: ${errors} file(s) with tracing::debug!/trace! inside mod tests blocks" >&2
    exit 1
fi

echo "OK: no tracing::debug!/trace! inside #[cfg(test)] mod tests blocks"