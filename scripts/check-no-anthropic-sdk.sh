#!/usr/bin/env bash
# Fail if any Anthropic SDK crate sneaks into Cargo.toml.
set -euo pipefail

if grep -nE '^(\s*)"?(anthropic[a-z0-9_-]*|claude[a-z0-9_-]*)"' Cargo.toml; then
    echo "ERROR: forbidden Anthropic SDK crate detected in Cargo.toml" >&2
    exit 1
fi

if grep -rnE 'anthropic[_:-][a-zA-Z]+|use\s+anthropic::' src/ tests/ examples/ 2>/dev/null; then
    echo "ERROR: forbidden Anthropic SDK import detected in source" >&2
    exit 1
fi

echo "OK: no Anthropic SDK references"
