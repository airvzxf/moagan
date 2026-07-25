#!/usr/bin/env bash
# Fail if any crate explicitly rejected by the architecture spec appears in Cargo.toml.
# See AGENTS.md for the rationale per crate.
set -euo pipefail

forbidden=(
    "secrecy"
    "axum"
    "hyper"
    "sqlx"
    "governor"
    "figment"
    "refinery"
    "askama"
    "handlebars"
    "lettre"
    "inquire"
    "time"
)

for crate in "${forbidden[@]}"; do
    if grep -nE "^${crate}\s*=" Cargo.toml; then
        echo "ERROR: forbidden crate '${crate}' found in Cargo.toml" >&2
        exit 1
    fi
done

echo "OK: no forbidden crates"
