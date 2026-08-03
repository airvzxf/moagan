#!/usr/bin/env bash
set -euo pipefail
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
  'grep -q "Sub-fase P" "$ROOT/docs/v0.3-status.md"'
  'grep -q "Decision table v0.3 patch" "$ROOT/docs/proposal-03-add-ons.md"'
)
for check in "${checks[@]}"; do
  bash -c "$check"
done
printf '%d smoke checks passed\n' "${#checks[@]}"
