#!/usr/bin/env bash
# Smoke checks for Phase P. Each entry is a single inline command;
# the loop exports ROOT so `$ROOT/...` resolves inside the subshell.
#
# Note: `set -uo pipefail` (no `-e`) lets one failed check be
# reported as FAIL without aborting the whole suite. The
# pre-2026-04 version used `set -euo pipefail`, which caused the
# first failing grep to silently kill the loop and the trailing
# `printf` to lie about how many checks ran.
#
# Usage:  ./scripts/smoke_phase_p.sh
# Exit:   0 when all checks pass, 1 otherwise.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export ROOT
checks=(
  'grep -q MergeSynthesizer "$ROOT/src/llm/role.rs"'
  'grep -q RecoveryExplainer "$ROOT/src/llm/role.rs"'
  'grep -q RationaleExtractor "$ROOT/src/llm/role.rs"'
  'test -s "$ROOT/src/llm/prompts/merge_synthesizer.md"'
  'test -s "$ROOT/src/llm/prompts/recovery_explainer.md"'
  'test -s "$ROOT/src/llm/prompts/rationale_extractor.md"'
  'grep -q acquire_many_owned "$ROOT/src/execution/parallelism.rs"'
  'grep -q TooManyPermits "$ROOT/src/execution/parallelism.rs"'
  'grep -q compress_or_report "$ROOT/src/storage/compression.rs"'
)
PASS=0
FAIL=0
FAILED_CHECKS=()
for check in "${checks[@]}"; do
  # Export ROOT so the subshell inherits it for the inline command.
  # Without this, single-quoted strings like "$ROOT/src/llm/role.rs"
  # interpolate as the literal text "$ROOT", which the subshell
  # cannot resolve and the check fails.
  if env ROOT="$ROOT" bash -c "$check"; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    FAILED_CHECKS+=("$check")
  fi
done

printf 'phase_p smoke: PASS=%d FAIL=%d TOTAL=%d\n' "$PASS" "$FAIL" "${#checks[@]}"
if [[ $FAIL -gt 0 ]]; then
  printf 'failed:\n'
  for c in "${FAILED_CHECKS[@]}"; do
    printf '  %s\n' "$c"
  done
  exit 1
fi
exit 0