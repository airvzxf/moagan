#!/usr/bin/env bash
# check-commit-msg.sh — enforce Conventional Commits on the subject line.
# Reference: https://www.conventionalcommits.org/
#
# Usage: scripts/check-commit-msg.sh <commit-msg-file>
#   <commit-msg-file> is the path passed by the commit-msg git hook.
#
# Format: <type>(<scope>)?(!)?: <subject>
#   type:    feat | fix | refactor | docs | test | chore | ci | build | perf
#   scope:   optional lowercase identifier (api, cli, llm, ...)
#   !:       optional, marks BREAKING CHANGE
#   subject: 3+ chars, no trailing period
#
# Merge / revert / fixup commits bypass the check automatically.

set -euo pipefail

msg_file="${1:?usage: $0 <commit-msg-file>}"
subject=$(head -n1 "$msg_file")

if [[ "$subject" =~ ^(Merge\ |Revert\ |fixup!\ |squash!\ |amend!\ ) ]]; then
  echo "OK: merge/revert/fixup commit — bypass"
  exit 0
fi

pattern='^(feat|fix|refactor|docs|test|chore|ci|build|perf)(\([a-z0-9_-]+\))?!?: .{3,}$'

if [[ ! "$subject" =~ $pattern ]]; then
  cat >&2 <<EOF
ERROR: commit subject does not follow Conventional Commits format.

  Got:      $subject
  Expected: <type>(<scope>)?(!)?: <subject>

Allowed types: feat, fix, refactor, docs, test, chore, ci, build, perf
Examples:
  feat(api): add /v1/users endpoint
  fix(cli): handle missing arg without panic
  refactor!: drop legacy pipeline (BREAKING CHANGE)
  docs: update README badge
  chore(deps): bump tokio to 1.41

Reference: https://www.conventionalcommits.org/
EOF
  exit 1
fi

echo "OK: conventional commit format"
