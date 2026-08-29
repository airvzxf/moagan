#!/usr/bin/env bash
# Fail if any `docs/` Markdown file shows the v0.12 legacy
# `[providers.<name>]` config schema in an *uncommented* context. The
# v0.13+ schema is `[[providers.<name>]]` (array-of-tables with
# `models = ["<id>", ...]`); the legacy form will be REMOVED in v0.15,
# so any active documentation example that still uses the legacy
# shape must be flagged here so a contributor can migrate it before
# the v0.15 cutover.
#
# Scope:
# - `docs/**/*.md` — documentation surfaces (ADR-0003,
#   migration guide, etc.). Uncommented legacy examples are a drift
#   signal.
# - `config.example.toml` — the canonical example file. The LEGACY
#   block at the bottom is intentionally commented out for
#   historical reference; this check ignores any line that starts
#   with `#` so the LEGACY block does not trip the gate.
#
# Excluded:
# - `tests/fixtures/config/v013_dual_mode/` — the fixtures exist
#   specifically to exercise the legacy form; the integration test
#   in `tests/integration_config_dual_mode.rs` loads them.
# - `src/` — production code; the dual-mode deserializer lives there
#   and naturally mentions both shapes. The check would false-positive
#   on every test fixture / comment that names "legacy".
#
# Heuristic: any line whose non-comment prefix matches
# `^[[:space:]]*models[[:space:]]*=[[:space:]]*\[\{` — the canonical
# legacy signature of `models = [{ id = "...", ... }]`. The new shape
# uses `models = ["..."]` (array of strings) which never matches this
# regex.
#
# Background: ADR-0003 (docs/adr/0003-config-schema-array-of-tables.md)
# §"When is legacy removed?".

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

errors=0
hits_output=""

scan_file() {
    local file="$1"
    local label="$2"
    # Skip entirely-commented lines (start with optional whitespace
    # then `#`) and strip inline `//` comments before pattern match.
    # The legacy signature is multi-line-tolerant: a single-line
    # `models = [{ id = ... }]` matches; a `models = [\n  { id = ... }]`
    # also matches because the open `[` followed by `{` is on the
    # first line.
    local hits
    hits=$(grep -nE '^[[:space:]]*models[[:space:]]*=[[:space:]]*\[\{' "$file" 2>/dev/null \
        | grep -vE '^[[:space:]]*[0-9]+:[[:space:]]*#' \
        || true)
    if [[ -n "$hits" ]]; then
        errors=$((errors + 1))
        hits_output+=$'\n'"--- $label: $file"$'\n'"$hits"$'\n'
    fi
}

# 1. Markdown docs.
while IFS= read -r -d '' file; do
    scan_file "$file" "docs"
done < <(find docs -type f -name '*.md' -print0 2>/dev/null)

# 2. config.example.toml (the LEGACY block is fully commented, so
# the line-prefix `#` filter strips it).
if [[ -f config.example.toml ]]; then
    scan_file "config.example.toml" "config.example.toml"
fi

if [[ "$errors" -gt 0 ]]; then
    printf '%s\n' 'config: legacy `[providers.X]` schema detected in uncommented docs / example config.'
    printf '%s\n' 'Migrate to v0.13 `[[providers.X]]` array-of-tables shape. See:'
    printf '%s\n' '  docs/adr/0003-config-schema-array-of-tables.md'
    printf '%s\n' '  docs/migrations/v0.12-to-v0.13-config.md'
    printf '%s\n' ''
    printf '%s\n' 'Hits:'
    printf '%s\n' "$hits_output"
    exit 1
fi

printf '%s\n' 'config: no legacy `[providers.X]` schema in docs/ or config.example.toml'
exit 0