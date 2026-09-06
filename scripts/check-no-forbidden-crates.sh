#!/usr/bin/env bash
# Fail if any crate explicitly rejected by the architecture spec appears in
# Cargo.toml OR if any forbidden crate reaches Cargo.lock as a transitive
# dependency without an ADR-driven allow-list entry.
#
# Scope:
#   1. Cargo.toml: every blanket-forbidden row (e.g. `time = "…"`) in any
#      section fails the build. Section-aware exceptions apply for the
#      dev-only `proptest` and optional-only `petgraph`.
#   2. Cargo.lock: every `[[package]]` whose `name = "<crate>"` matches a
#      blanket-forbidden crate fails UNLESS the crate is on the
#      `transitive_allowlist`. The allow-list is the documented escape
#      hatch for unavoidable transitive pulls (e.g. `hyper` via
#      `reqwest + rustls-tls`, `time` via `jsonschema 0.17.1` / `zip 2`).
#      Adding an entry requires amending ADR-0001.
#
# See AGENTS.md for the rationale per crate, and
# docs/adr/0001-no-go-list-policy.md for the differentiated verdicts on
# `comfy-table`, `proptest`, `petgraph`, and the transitive allow-list.
set -euo pipefail

CARGO_TOML="${CARGO_TOML:-Cargo.toml}"
CARGO_LOCK="${CARGO_LOCK:-Cargo.lock}"
errors=0

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
# section-aware scan. NOT applied to Cargo.lock: transitive proptest
# is fine because proptest only flows through `[dev-dependencies]`
# and the release binary does not link it.
prod_only_forbidden=(
    "proptest"
)

# Crates that must appear with `optional = true` on the same
# declaration line; otherwise rejected. Default build does not pull
# them, keeping the release binary footprint unchanged.
optional_only=(
    "petgraph"
)

# Crates that ARE on the blanket-forbidden list but are currently
# unavoidable as transitive deps. Each entry requires a one-line
# comment pointing at the parent crate that pulls it in. A future PR
# that drops the parent crate MUST remove the corresponding entry
# here in the same change (otherwise the script silently keeps the
# violation green).
#
# Order matters for the warning printer; keep alphabetical.
transitive_allowlist=(
    # `hyper` enters via reqwest 0.12 + rustls-tls → hyper-rustls, and
    # via wiremock 0.6 (dev-dep) → hyper-util. Dropping reqwest would
    # require replacing every HTTP call site; tracked separately.
    "hyper"
    # `time` enters via jsonschema 0.17.1 and zip 2.x. Replacing either
    # crate is a multi-week migration; tracked in ADR-0001.
    "time"
)

if [[ ! -f "${CARGO_TOML}" ]]; then
    echo "ERROR: ${CARGO_TOML} not found" >&2
    exit 1
fi

# Track the current Cargo section header so per-section rules can fire.
# `target.<triple>.dependencies` and `build-dependencies` count as
# production sections for the purpose of this guard: anything that
# touches the release binary is treated as runtime.
in_dev_deps=0
section=""

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

# ---------------------------------------------------------------------------
# Phase 2: Cargo.lock transitive-resolution scan. Each `[[package]]` block
# has a `name = "<crate>"` line. A blanket-forbidden name in the lockfile
# that is NOT on the transitive_allowlist is a regression that the old
# script would have missed entirely.
# ---------------------------------------------------------------------------
if [[ -f "${CARGO_LOCK}" ]]; then
    while IFS= read -r line; do
        # Match only the package-name declaration, not check-sum lines.
        if [[ "${line}" =~ ^name[[:space:]]*=[[:space:]]*\"([a-zA-Z0-9_-]+)\"[[:space:]]*$ ]]; then
            pkg="${BASH_REMATCH[1]}"
            for crate in "${blanket_forbidden[@]}"; do
                if [[ "${pkg}" == "${crate}" ]]; then
                    on_allowlist=0
                    for allowed in "${transitive_allowlist[@]}"; do
                        if [[ "${allowed}" == "${crate}" ]]; then
                            on_allowlist=1
                            break
                        fi
                    done
                    if [[ "${on_allowlist}" -eq 0 ]]; then
                        echo "ERROR: forbidden crate '${crate}' reached ${CARGO_LOCK} as a transitive dependency (no allow-list entry). Add it to transitive_allowlist with a comment pointing at the parent crate, OR drop the parent crate. See ADR-0001." >&2
                        errors=$((errors + 1))
                    fi
                fi
            done
        fi
    done < "${CARGO_LOCK}"
else
    echo "WARN: ${CARGO_LOCK} not found; skipping transitive-resolution scan" >&2
fi

if [[ "${errors}" -ne 0 ]]; then
    echo "ERROR: ${errors} forbidden crate violation(s) detected" >&2
    exit 1
fi

echo "OK: no forbidden crates in Cargo.toml or Cargo.lock"