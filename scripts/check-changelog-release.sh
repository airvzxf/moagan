#!/usr/bin/env bash
# Guard: a release bump must rename the CHANGELOG [Unreleased] heading.
#
# Three assertions, wired into `make guard-deps` (T0 tier):
#
#   1. The version in Cargo.toml has a matching `## [X.Y.Z] - YYYY-MM-DD`
#      heading in CHANGELOG.md. (Tightened per validate-788 finding: the
#      original OR-clause — "or only `[Unreleased]` above the newest
#      release heading" — would let the pre-fix buggy state pass, since
#      that is exactly the case the guard exists to prevent.)
#
#   2. At most one `## [Unreleased]` heading.
#
#   3. Every `## [X.Y.Z]` heading has a matching `[X.Y.Z]:
#      https://github.com/...compare/...` footnote.
#
# Exit code: 0 on success, 1 on any failure (each failure prints a
# self-explanatory error to stderr).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

# Extract the version from Cargo.toml. The match is anchored to the
# top-level `[package]` table — first `^version =` wins.
CARGO_VERSION="$(grep -E '^version\s*=' Cargo.toml | head -1 | sed -E 's/^version\s*=\s*"([^"]+)".*/\1/')"
if [ -z "${CARGO_VERSION}" ]; then
    echo "ERROR: cannot extract version from Cargo.toml" >&2
    exit 1
fi

CHANGELOG="CHANGELOG.md"
if [ ! -f "${CHANGELOG}" ]; then
    echo "ERROR: ${CHANGELOG} not found" >&2
    exit 1
fi

# Assertion 1: Cargo.toml version MUST have a matching ## [X.Y.Z] heading.
if ! grep -qE "^## \[${CARGO_VERSION//./\\.}\] " "${CHANGELOG}"; then
    echo "ERROR: Cargo.toml version ${CARGO_VERSION} has no matching ## [${CARGO_VERSION}] heading in ${CHANGELOG}" >&2
    echo "HINT: the release commit must rename '## [Unreleased]' to '## [${CARGO_VERSION}] - YYYY-MM-DD'." >&2
    exit 1
fi

# Assertion 2: at most one `## [Unreleased]` heading.
UNRELEASED_COUNT="$(grep -cE '^## \[Unreleased\]' "${CHANGELOG}" || true)"
if [ "${UNRELEASED_COUNT}" -gt 1 ]; then
    echo "ERROR: ${CHANGELOG} has ${UNRELEASED_COUNT} '## [Unreleased]' headings (expected at most 1)" >&2
    exit 1
fi

# Assertion 3: every `## [X.Y.Z]` heading has a matching compare-link footnote.
# Extract all version headings (skipping [Unreleased]).
mapfile -t HEADING_VERSIONS < <(grep -E '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' "${CHANGELOG}" | sed -E 's/^## \[([0-9]+\.[0-9]+\.[0-9]+)\].*/\1/')

MISSING=""
for v in "${HEADING_VERSIONS[@]}"; do
    if ! grep -qE "^\[${v//./\\.}\]: " "${CHANGELOG}"; then
        MISSING="${MISSING} ${v}"
    fi
done
if [ -n "${MISSING}" ]; then
    echo "ERROR: ${CHANGELOG} is missing compare-link footnotes for:${MISSING}" >&2
    exit 1
fi

echo "OK: CHANGELOG.md release guard passed (version ${CARGO_VERSION}, ${#HEADING_VERSIONS[@]} version headings, all footnotes present)"
