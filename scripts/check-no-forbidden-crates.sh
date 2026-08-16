#!/usr/bin/env bash
# Fail if any crate explicitly rejected by the architecture spec appears in Cargo.toml.
# See AGENTS.md for the rationale per crate, and
# docs/adr/0001-no-go-list-policy.md for the differentiated verdicts on
# `comfy-table`, `proptest`, and `petgraph`.
set -euo pipefail

CARGO_TOML="${CARGO_TOML:-Cargo.toml}"
if [[ ! -f "${CARGO_TOML}" ]]; then
    echo "ERROR: ${CARGO_TOML} not found" >&2
    exit 1
fi

# Crates that are blanket-forbidden regardless of section. A row
# matching `^crate = …` in any `[dependencies]`, `[dev-dependencies]`,
# `[build-dependencies]`, or `[target.*.dependencies]` table is
# rejected. `cargo-features` lines, comments, and unrelated matches
# are filtered out.
blanket_forbidden=(
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
    "comfy-table"
)

# Crates forbidden ONLY in the runtime `[dependencies]` (and target /
# build) sections; allowed in `[dev-dependencies]`. Enforced by a
# section-aware scan.
prod_only_forbidden=(
    "proptest"
)

# Crates that must appear with `optional = true` on the same
# declaration line; otherwise rejected. Default build does not pull
# them, keeping the release binary footprint unchanged.
optional_only=(
    "petgraph"
)

# Track the current Cargo section header so per-section rules can fire.
# `target.<triple>.dependencies` and `build-dependencies` count as
# production sections for the purpose of this guard: anything that
# touches the release binary is treated as runtime.
in_dev_deps=0
section=""
errors=0

while IFS= read -r line; do
    # Skip blank lines and comments early.
    [[ -z "${line// }" ]] && continue
    [[ "${line}" =~ ^[[:space:]]*# ]] && continue

    if [[ "${line}" =~ ^\[(.+)\][[:space:]]*$ ]]; then
        section="${BASH_REMATCH[1]}"
        if [[ "${section}" == "dev-dependencies" ]]; then
            in_dev_deps=1
        else
            in_dev_deps=0
        fi
        continue
    fi

    # Blanket-forbidden crates: any line that starts with `crate =` (or
    # `crate = {`) is rejected. The earlier `forbidden` script only
    # matched `^crate =`; we keep that scope to avoid false positives on
    # unrelated identifier matches.
    for crate in "${blanket_forbidden[@]}"; do
        if [[ "${line}" =~ ^${crate}[[:space:]]*= ]]; then
            echo "ERROR: forbidden crate '${crate}' found in ${CARGO_TOML} (section: [${section}]): ${line}" >&2
            errors=$((errors + 1))
        fi
    done

    # Production-only forbidden: allowed in [dev-dependencies], rejected
    # everywhere else.
    if [[ "${in_dev_deps}" -eq 0 ]]; then
        for crate in "${prod_only_forbidden[@]}"; do
            if [[ "${line}" =~ ^${crate}[[:space:]]*= ]]; then
                echo "ERROR: '${crate}' is dev-deps only per ADR 0001; runtime [${section}] row is forbidden: ${line}" >&2
                errors=$((errors + 1))
            fi
        done
    fi

    # Optional-only: must contain `optional = true` on the same
    # declaration line. Applies in every section (dev-deps is fine too
    # if someone wants it as an optional dev-dep).
    for crate in "${optional_only[@]}"; do
        if [[ "${line}" =~ ^${crate}[[:space:]]*= ]]; then
            if ! [[ "${line}" =~ optional[[:space:]]*=[[:space:]]*true ]]; then
                echo "ERROR: '${crate}' is allowed only as optional = true per ADR 0001 (section: [${section}]): ${line}" >&2
                errors=$((errors + 1))
            fi
        fi
    done
done < "${CARGO_TOML}"

if [[ "${errors}" -ne 0 ]]; then
    echo "ERROR: ${errors} forbidden crate declaration(s) in ${CARGO_TOML}" >&2
    exit 1
fi

echo "OK: no forbidden crates"