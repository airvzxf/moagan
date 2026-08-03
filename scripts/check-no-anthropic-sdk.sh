#!/usr/bin/env bash
# Fail if any Anthropic SDK crate sneaks into Cargo.toml.
set -euo pipefail

if grep -nE '^(\s*)"?(anthropic[a-z0-9_-]*|claude[a-z0-9_-]*)"' Cargo.toml; then
    echo "ERROR: forbidden Anthropic SDK crate detected in Cargo.toml" >&2
    exit 1
fi

# Source: catch `use anthropic::*` / `use claude::*` Rust import lines.
# The regex is anchored to `^use\s+` so it does NOT match test names
# (`anthropic_wire_roundtrips`), redact patterns (`[REDACTED:anthropic_key]`),
# config strings (`hard_incompatibilities: vec!["anthropic-sdk".to_owned()]`),
# or comment text. Only a real Rust import is forbidden.
# The `examples/` path is intentionally absent: the repo does not carry
# an examples directory, and grep exits 2 when a path is missing — that
# exit code falls through the `if` test and turns a future real Anthropic
# SDK import into a silent "OK". The script now scans only the directories
# that actually exist on disk.
if grep -rnE '^use\s+(anthropic|claude)(_sdk)?::' src/ tests/ 2>/dev/null; then
    echo "ERROR: forbidden Anthropic SDK import detected in source" >&2
    exit 1
fi

echo "OK: no Anthropic SDK references"
