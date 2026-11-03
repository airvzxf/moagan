#!/usr/bin/env bash
# Executable spec for scripts/cargo-audit-issue.sh.
#
# A stub `gh` on PATH records every call and answers `gh issue list`
# with a canned listing, so each case checks which issue operations the
# script performs for one (report, open issues) pair. No network.
#
# Wired into `make guard-deps` (T0). Exit 0 when every case passes.
set -uo pipefail

SCRIPT="${SCRIPT:-scripts/cargo-audit-issue.sh}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
failures=0
cases=0

mkdir -p "${work}/bin"
cat >"${work}/bin/gh" <<'STUB'
#!/usr/bin/env bash
if [[ "$1 $2" == "issue list" ]]; then
    printf '%s' "${GH_ISSUES_JSON}"
    exit 0
fi
echo "$1 $2 $3" >>"${GH_LOG}"
STUB
chmod +x "${work}/bin/gh"

TITLE='cargo-audit: RustSec advisories on main'
CLEAN='{"vulnerabilities":{"count":0,"list":[]}}'
TWO='{"vulnerabilities":{"count":2,"list":[
  {"id":"RUSTSEC-2026-0398","crate":"zstd-safe","version":"7.3.0","title":"a | b","patched":["^7.4.0"]},
  {"id":"RUSTSEC-2026-0391","crate":"zstd","version":"0.13.3","title":"c","patched":[]}]}}'
ONE='{"vulnerabilities":{"count":1,"list":[
  {"id":"RUSTSEC-2026-0391","crate":"zstd","version":"0.13.3","title":"c","patched":[">=0.14.3"]}]}}'
issue() { # issue <number> <ids> [title]
    jq -nc --argjson n "$1" --arg ids "$2" --arg t "${3:-${TITLE}}" \
        '[{number: $n, title: $t, body: "text\n<!-- cargo-audit-ids: \($ids) -->"}]'
}

# check <name> <expected rc> <expected gh calls> <report json> <issues json>
check() {
    local name="$1" want_rc="$2" want_calls="$3" report="$4" issues="$5"
    local got_rc got_calls
    printf '%s' "${report}" >"${work}/audit.json"
    : >"${work}/gh.log"
    PATH="${work}/bin:${PATH}" GH_LOG="${work}/gh.log" GH_ISSUES_JSON="${issues}" \
        GH_REPO=owner/repo bash "${SCRIPT}" sync "${work}/audit.json" RUN >/dev/null 2>&1
    got_rc=$?
    got_calls="$(cat "${work}/gh.log")"
    cases=$((cases + 1))
    if [[ "${got_rc}" != "${want_rc}" || "${got_calls}" != "${want_calls}" ]]; then
        failures=$((failures + 1))
        echo "FAIL  ${name}" >&2
        echo "      want rc=${want_rc} calls=[${want_calls//$'\n'/; }]" >&2
        echo "      got  rc=${got_rc} calls=[${got_calls//$'\n'/; }]" >&2
    else
        echo "ok    ${name}"
    fi
}

check "clean, no issue: nothing" 0 "" "${CLEAN}" '[]'
check "clean, issue open: comment + close" 0 $'issue comment 7\nissue close 7' \
    "${CLEAN}" "$(issue 7 RUSTSEC-2026-0391)"
check "advisories, no issue: create" 0 $'issue create --title' "${TWO}" '[]'
check "advisories, same ids (any order): nothing" 0 "" \
    "${TWO}" "$(issue 7 RUSTSEC-2026-0391,RUSTSEC-2026-0398)"
check "advisories, other ids: edit + comment" 0 $'issue edit 7\nissue comment 7' \
    "${ONE}" "$(issue 7 RUSTSEC-2026-0391,RUSTSEC-2026-0398)"
check "an issue with another title is ignored" 0 $'issue create --title' \
    "${ONE}" "$(issue 9 RUSTSEC-2026-0391 'cargo-audit: something else')"
check "invalid report: exit 2, no calls" 2 "" 'not json' '[]'
check "report without list: exit 2, no calls" 2 "" '{"vulnerabilities":{"count":0}}' '[]'

# The body escapes `|` in titles, renders `none` for no patch and
# carries the sorted id marker the next run compares against.
printf '%s' "${TWO}" >"${work}/audit.json"
body="$(bash "${SCRIPT}" body "${work}/audit.json" RUN)"
cases=$((cases + 1))
if grep -qF 'a \| b' <<<"${body}" \
    && grep -qF '| `zstd` | 0.13.3 | none | c |' <<<"${body}" \
    && grep -qF '<!-- cargo-audit-ids: RUSTSEC-2026-0391,RUSTSEC-2026-0398 -->' <<<"${body}" \
    && grep -qF 'matches 2 RustSec advisories' <<<"${body}"; then
    echo "ok    body renders table and marker"
else
    failures=$((failures + 1))
    echo "FAIL  body renders table and marker" >&2
    echo "${body}" >&2
fi

echo "test-cargo-audit-issue: $((cases - failures))/${cases} passed"
[[ "${failures}" -eq 0 ]]
