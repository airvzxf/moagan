#!/usr/bin/env bash
# Keep the nightly cargo-audit result in ONE tracking issue.
#
# The nightly run used to fail on main every day until an advisory was
# fixed, so a single advisory turned into a week of red runs. Now the
# scheduled run stays green and this script reports the advisories in
# one issue instead:
#
#   advisories found, no open issue                 -> create the issue
#   advisories found, issue open, same advisory ids -> do nothing
#   advisories found, issue open, other advisory ids -> rewrite + comment
#   clean report, issue open                        -> comment + close
#   clean report, no open issue                     -> do nothing
#
# Usage:
#   cargo-audit-issue.sh body <audit.json> <run-url>   # print the markdown
#   cargo-audit-issue.sh sync <audit.json> <run-url>   # create/update/close
#
# <audit.json> is the reduced report written by cargo-audit.yml:
#   { "vulnerabilities": { "count": N,
#       "list": [ { "id", "crate", "version", "title", "patched" } ] } }
#
# Exit 2 when the report is not valid: a broken tool run (advisory-DB
# fetch failure, cargo-audit crash) must keep the workflow red.
# `sync` needs `gh` with GH_TOKEN and GH_REPO in the environment.
# Spec: scripts/test-cargo-audit-issue.sh (wired into `make guard-deps`).
set -euo pipefail

TITLE='cargo-audit: RustSec advisories on main'
MARKER='cargo-audit-ids:'

usage() {
    echo "usage: $0 body|sync <audit.json> <run-url>" >&2
    exit 64
}

[[ $# -eq 3 ]] || usage
mode="$1" report="$2" run_url="$3"

if ! count="$(jq -er '.vulnerabilities.count | numbers' "${report}" 2>/dev/null)" \
    || ! jq -e '.vulnerabilities.list | arrays' "${report}" >/dev/null 2>&1; then
    echo "cargo-audit-issue: invalid report: ${report}" >&2
    exit 2
fi
ids="$(jq -r '[.vulnerabilities.list[].id] | sort | unique | join(",")' "${report}")"

render_body() {
    jq -r --arg run "${run_url}" --arg marker "${MARKER}" --arg ids "${ids}" '
        def cell: tostring | gsub("\\|"; "\\|") | gsub("\n"; " ");
        .vulnerabilities as $v
        | "The nightly `cargo-audit` run on `main` matches \($v.count) "
          + "RustSec advisor\(if $v.count == 1 then "y" else "ies" end) in `Cargo.lock`.\n\n"
          + "| Advisory | Crate | Version | Patched | Title |\n"
          + "|---|---|---|---|---|\n"
          + ($v.list | sort_by(.id) | map(
                "| [\(.id)](https://rustsec.org/advisories/\(.id).html) "
                + "| `\(.crate)` | \(.version) "
                + "| \((.patched // []) | if length == 0 then "none" else map("`\(.)`") | join(", ") end) "
                + "| \(.title | cell) |"
            ) | join("\n"))
          + "\n\nLatest run: \($run)\n\n"
          + "To fix: run `cargo update -p <crate>` when a semver-compatible patch exists, "
          + "otherwise bump the dependency. If the advisory does not apply to moagan, add "
          + "its id with a reason to `.cargo/audit.toml`. This issue closes itself on the "
          + "first clean nightly run.\n\n"
          + "<!-- \($marker) \($ids) -->"
    ' "${report}"
}

case "${mode}" in
    body)
        render_body
        exit 0
        ;;
    sync) ;;
    *) usage ;;
esac

: "${GH_REPO:?GH_REPO must be set}"

# Exact title match over open issues (the search index can lag behind
# a just-created issue, so filter the plain listing instead).
open_issue="$(gh issue list --state open --limit 1000 --json number,title,body \
    | jq -c --arg t "${TITLE}" '[.[] | select(.title == $t)] | sort_by(.number) | first // empty')"

if [[ "${count}" -eq 0 ]]; then
    if [[ -z "${open_issue}" ]]; then
        echo "cargo-audit-issue: clean report, no open issue: nothing to do"
        exit 0
    fi
    number="$(jq -r '.number' <<<"${open_issue}")"
    gh issue comment "${number}" --body "The nightly \`cargo-audit\` run is clean again: ${run_url}"
    gh issue close "${number}" --reason completed
    echo "cargo-audit-issue: closed #${number}"
    exit 0
fi

body="$(render_body)"

if [[ -z "${open_issue}" ]]; then
    gh issue create --title "${TITLE}" --label security --label dependencies --body "${body}"
    echo "cargo-audit-issue: created the tracking issue (${ids})"
    exit 0
fi

number="$(jq -r '.number' <<<"${open_issue}")"
old_ids="$(jq -r '.body' <<<"${open_issue}" \
    | sed -n "s/.*<!-- ${MARKER} \(.*\) -->.*/\1/p" | tail -n 1)"

if [[ "${old_ids}" == "${ids}" ]]; then
    echo "cargo-audit-issue: #${number} already lists ${ids}: nothing to do"
    exit 0
fi

gh issue edit "${number}" --body "${body}"
gh issue comment "${number}" --body "The advisory set changed: was \`${old_ids:-none}\`, now \`${ids}\`. ${run_url}"
echo "cargo-audit-issue: updated #${number} (${old_ids:-none} -> ${ids})"
