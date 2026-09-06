#!/usr/bin/env bash
# Fail if any Rust source file builds a `/tmp/moagan-*` directory
# manually via `std::env::temp_dir().join(...)` instead of using
# `tempfile::TempDir` (whose `Drop` impl cleans up on the panic
# path too). The pattern caught here is the historical source of
# the 19 GB tmpfs leak documented in
# docs/discovery-validation-research-2026-08-13.md.
#
# Allowed forms:
#   - `tempfile::Builder::new().prefix("moagan-…").tempdir()`
#   - `tempfile::tempdir()` with a `moagan-` prefix from the caller
#   - `with_moagan_home(label, |home| …)` (the canonical helper in
#     `src/test_support.rs`, which itself uses `TempDir`)
#   - `cli/discover.rs::build_canonical_for_resume_pipeline` no
#     longer creates any tempdir (it takes `&MoaganHome`), so no
#     exception is needed for that file.
#
# False-positive caveat: this is a pattern guard, not a semantics
# guard. If a future change genuinely needs a persistent
# `moagan-*` dir under `/tmp` (e.g. operator-visible artifact),
# prefer `tempfile::Builder::keep(true)` so the lifetime is
# explicit at the call site.
set -euo pipefail

ROOTS=("src" "tests")
# Two patterns are matched:
#   1. `std::env::temp_dir().join("moagan-…")` — runtime-resolved path
#   2. `PathBuf::from("/tmp/moagan-…")` — hardcoded literal path
# Both create a `/tmp/moagan-*` directory that survives test runs and
# pollutes the runner's tmpfs. Use `tempfile::TempDir` (or
# `tempfile::Builder`) so the directory is removed on Drop (success
# or panic). Issue #719 closed the second pattern in gate.rs; this
# guard now catches both.
PATTERN='(std::env::temp_dir\(\).*moagan-|PathBuf::from\(\s*"/tmp/moagan-)'

matches=$(grep -rnE "${PATTERN}" "${ROOTS[@]}" 2>/dev/null || true)

if [[ -n "${matches}" ]]; then
    echo "ERROR: manual /tmp/moagan-* tempdir pattern detected:" >&2
    echo "${matches}" >&2
    echo >&2
    echo "Replace with tempfile::TempDir (or tempfile::Builder) so the" >&2
    echo "directory is removed on Drop (success or panic)." >&2
    echo "See scripts/check-no-tempdir-leaks.sh for the rationale." >&2
    exit 1
fi

echo "OK: no manual /tmp/moagan-* tempdir sites"
