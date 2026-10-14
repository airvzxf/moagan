#!/usr/bin/env bash
# Executable spec for scripts/ci-cache-prune.sh (ADR-0014).
#
# The prune script is a pure filter: it reads the Actions cache listing
# on stdin (one `id<TAB>ref<TAB>createdAt<TAB>key` line per entry) and
# prints the ids to delete, one per line, in ascending numeric order.
# Its arguments are the rust-cache shared keys still in use. On main,
# only the newest entry of each live shared key survives; every entry
# of any other ref, and every entry of a key nobody uses, is deleted.
#
# Wired into `make guard-deps` (T0). Exit 0 when every case passes.
set -uo pipefail

PRUNE="${PRUNE:-scripts/ci-cache-prune.sh}"
failures=0
cases=0

# check <name> <expected rc> <expected stdout> <stdin> [args...]
check() {
    local name="$1" want_rc="$2" want_out="$3" input="$4"
    shift 4
    local got_out got_rc
    got_out="$(printf '%s' "${input}" | bash "${PRUNE}" "$@" 2>/dev/null)"
    got_rc=$?
    cases=$((cases + 1))
    if [[ "${got_rc}" != "${want_rc}" || "${got_out}" != "${want_out}" ]]; then
        failures=$((failures + 1))
        echo "FAIL  ${name}" >&2
        echo "      want rc=${want_rc} stdout=[${want_out//$'\n'/ }]" >&2
        echo "      got  rc=${got_rc} stdout=[${got_out//$'\n'/ }]" >&2
    else
        echo "ok    ${name}"
    fi
}

T=$'\t'
ENV_A='d0ea4c79'
ENV_B='5b1f00aa'
LOCK_OLD='22250ded'
LOCK_NEW='3dbaf575'

# The 12 entries the repository held on 2026-10-14 (ids shortened),
# written by the pre-ADR-0014 cache layout. None of their shared keys
# is live any more, so all of them go.
LEGACY="\
101${T}refs/heads/main${T}2026-10-14T20:05:00Z${T}v0-rust-e2e-network-on-tag-Linux-x64-${ENV_A}-${LOCK_NEW}
102${T}refs/heads/main${T}2026-10-14T20:05:01Z${T}v0-rust-moagan-test-ignored-Linux-x64-${ENV_A}-${LOCK_NEW}
103${T}refs/heads/main${T}2026-10-14T02:13:00Z${T}v0-rust-moagan-test-ignored-Linux-x64-${ENV_A}-${LOCK_OLD}
104${T}refs/heads/main${T}2026-10-14T02:11:00Z${T}v0-rust-e2e-network-on-tag-Linux-x64-${ENV_A}-${LOCK_OLD}
105${T}refs/heads/main${T}2026-10-14T17:21:00Z${T}v0-rust-moagan-rust-shared-v1-Linux-x64-${ENV_A}-${LOCK_NEW}
106${T}refs/heads/main${T}2026-10-14T17:22:00Z${T}v0-rust-sync-Linux-x64-${ENV_A}-${LOCK_NEW}
107${T}refs/pull/1045/merge${T}2026-10-14T17:23:00Z${T}v0-rust-sync-Linux-x64-${ENV_A}-${LOCK_NEW}
108${T}refs/pull/1044/merge${T}2026-10-14T17:23:01Z${T}v0-rust-sync-Linux-x64-${ENV_A}-${LOCK_NEW}
109${T}refs/heads/main${T}2026-10-13T22:41:00Z${T}v0-rust-sync-Linux-x64-${ENV_A}-${LOCK_OLD}
110${T}refs/heads/main${T}2026-10-13T22:40:00Z${T}v0-rust-moagan-rust-shared-v1-Linux-x64-${ENV_A}-${LOCK_OLD}
111${T}refs/pull/1046/merge${T}2026-10-14T17:14:00Z${T}v0-rust-sync-Linux-x64-${ENV_A}-${LOCK_NEW}
112${T}refs/pull/1046/merge${T}2026-10-14T17:12:00Z${T}v0-rust-moagan-rust-shared-v1-Linux-x64-${ENV_A}-${LOCK_NEW}
"

check "legacy_entries_are_all_deleted_once_no_live_key_matches_them" 0 \
"101
102
103
104
105
106
107
108
109
110
111
112" "${LEGACY}" moagan-dev moagan-release

# Steady state after ADR-0014: one dev entry and one release entry on
# main, superseded by a Cargo.lock bump.
STEADY="\
201${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_OLD}
202${T}refs/heads/main${T}2026-10-16T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
203${T}refs/heads/main${T}2026-10-15T11:00:00Z${T}v0-rust-moagan-release-Linux-x64-${ENV_A}-${LOCK_OLD}
"

check "main_keeps_the_newest_entry_of_each_live_key_and_drops_the_superseded_one" 0 \
"201" "${STEADY}" moagan-dev moagan-release

check "a_lone_entry_per_live_key_is_never_deleted" 0 "" \
"202${T}refs/heads/main${T}2026-10-16T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
203${T}refs/heads/main${T}2026-10-15T11:00:00Z${T}v0-rust-moagan-release-Linux-x64-${ENV_A}-${LOCK_OLD}
" moagan-dev moagan-release

check "a_toolchain_bump_supersedes_the_old_environment_hash_too" 0 "301" \
"301${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
302${T}refs/heads/main${T}2026-10-17T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_B}-${LOCK_NEW}
" moagan-dev

check "entries_of_any_other_ref_are_deleted_even_for_a_live_key" 0 \
"401
402" \
"401${T}refs/pull/1051/merge${T}2026-10-16T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
402${T}refs/heads/feature-x${T}2026-10-16T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
403${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
" moagan-dev

check "a_key_that_only_starts_with_a_live_key_is_not_live" 0 "502" \
"501${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
502${T}refs/heads/main${T}2026-10-16T10:00:00Z${T}v0-rust-moagan-dev-extra-Linux-x64-${ENV_A}-${LOCK_NEW}
" moagan-dev

check "each_runner_os_and_arch_keeps_its_own_newest_entry" 0 "" \
"601${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
602${T}refs/heads/main${T}2026-10-16T10:00:00Z${T}v0-rust-moagan-dev-Linux-arm64-${ENV_A}-${LOCK_NEW}
" moagan-dev

check "equal_creation_times_keep_the_highest_id" 0 "701" \
"702${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_OLD}
701${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
" moagan-dev

check "ids_are_printed_in_ascending_numeric_order" 0 \
"9
80
700" \
"700${T}refs/pull/1/merge${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
9${T}refs/pull/2/merge${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
80${T}refs/pull/3/merge${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_NEW}
" moagan-dev

SHUFFLED="$(printf '%s' "${STEADY}" | tac)
"
check "the_result_does_not_depend_on_input_order" 0 "201" "${SHUFFLED}" moagan-dev moagan-release

check "blank_lines_are_ignored" 0 "201" "
${STEADY}
" moagan-dev moagan-release

check "an_empty_listing_deletes_nothing" 0 "" "" moagan-dev

check "no_live_keys_is_a_usage_error_and_deletes_nothing" 2 "" "${STEADY}"

check "a_malformed_line_aborts_and_deletes_nothing" 3 "" \
"201${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_OLD}
garbage line without tabs
" moagan-dev

check "a_non_numeric_id_aborts_and_deletes_nothing" 3 "" \
"7.55e+09${T}refs/pull/1/merge${T}2026-10-15T10:00:00Z${T}v0-rust-moagan-dev-Linux-x64-${ENV_A}-${LOCK_OLD}
" moagan-dev

check "a_key_outside_the_rust_cache_layout_on_main_is_deleted" 0 "801" \
"801${T}refs/heads/main${T}2026-10-15T10:00:00Z${T}some-other-cache-key
" moagan-dev

if [[ "${failures}" -ne 0 ]]; then
    echo "ERROR: ${failures} of ${cases} ci-cache-prune case(s) failed." >&2
    exit 1
fi
echo "OK: ${cases} ci-cache-prune cases passed"
