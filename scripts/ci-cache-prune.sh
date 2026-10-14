#!/usr/bin/env bash
# Decide which GitHub Actions cache entries to delete (ADR-0014).
#
# Usage: scripts/ci-cache-prune.sh SHARED_KEY [SHARED_KEY...] < listing
#
# stdin:  one entry per line, `id<TAB>ref<TAB>createdAt<TAB>key`, as
#         produced by `gh cache list --json id,ref,createdAt,key`.
#         Blank lines are ignored.
# args:   the rust-cache shared keys still in use (the `shared-key:`
#         values of the workflows). At least one is required.
# stdout: the ids to delete, one per line, ascending numeric order.
#
# Rules (pure function of stdin + args, no network):
#   1. An entry whose ref is not `refs/heads/main` is deleted. Only
#      main writes the cache (rust-setup's `save-if`), so anything else
#      is a leftover.
#   2. A main entry is live when its key is
#      `v0-rust-<K>-<os>-<arch>-<envhash>-<lockhash>` with <K> exactly
#      one of the args. Any other main entry is deleted.
#   3. Live entries are grouped by `<K>-<os>-<arch>`; each group keeps
#      its newest entry (greatest createdAt, then greatest id) and the
#      rest are deleted (superseded by a Cargo.lock, Cargo.toml or
#      toolchain change).
#
# Exit codes: 0 ok; 2 no shared key given; 3 malformed input line or
# non-numeric id. On any non-zero exit nothing is printed on stdout,
# so a caller that deletes what it reads deletes nothing.
set -euo pipefail

if [[ "$#" -eq 0 ]]; then
    echo "usage: ci-cache-prune.sh SHARED_KEY [SHARED_KEY...] < listing" >&2
    exit 2
fi

# The verdicts are collected first and printed only once awk has read
# the whole listing, so a malformed line anywhere leaves stdout empty.
verdicts="$(awk -F '\t' -v live="$*" '
    BEGIN {
        n = split(live, keys, " ")
        for (i = 1; i <= n; i++) live_key[keys[i]] = 1
    }
    /^[[:space:]]*$/ { next }
    NF != 4 || $1 !~ /^[0-9]+$/ {
        printf "ci-cache-prune.sh: malformed line %d: %s\n", NR, $0 > "/dev/stderr"
        malformed = 1
        exit 3
    }
    {
        id = $1; ref = $2; created = $3; key = $4
        if (ref != "refs/heads/main") { doomed[id] = 1; next }
        # v0-rust-<K>-<os>-<arch>-<envhash>-<lockhash>
        m = split(key, seg, "-")
        if (m < 7 || seg[1] != "v0" || seg[2] != "rust") { doomed[id] = 1; next }
        shared = seg[3]
        for (i = 4; i <= m - 4; i++) shared = shared "-" seg[i]
        if (!(shared in live_key)) { doomed[id] = 1; next }
        group = shared "-" seg[m - 3] "-" seg[m - 2]
        if (!(group in keep)) { keep[group] = id; keep_at[group] = created; next }
        newer = created > keep_at[group] \
            || (created == keep_at[group] && id + 0 > keep[group] + 0)
        if (newer) {
            doomed[keep[group]] = 1
            keep[group] = id; keep_at[group] = created
        } else {
            doomed[id] = 1
        }
    }
    END {
        if (malformed) exit 3
        for (id in doomed) print id
    }
')"

if [[ -n "${verdicts}" ]]; then
    printf '%s\n' "${verdicts}" | sort -n
fi
