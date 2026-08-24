#!/usr/bin/env bash
# gauntlet.sh — single-command validation gauntlet for moagan.
#
# Runs all the checks the AGENTS.md "Validation gauntlet" and the
# global AGENTS.md "Commit signing" / "Pre-push" rules require,
# plus the smoke gates and the SPECIFIC sanity gates we have
# accumulated across the discovery/modelling/integration phases.
#
# Usage:
#   ./scripts/gauntlet.sh                    # full gauntlet
#   ./scripts/gauntlet.sh --fast             # skip minimax smoke (no API key)
#   ./scripts/gauntlet.sh --skip-smoke       # skip both smoke gates
#   ./scripts/gauntlet.sh --skip-clippy      # skip clippy (CI does clippy)
#   ./scripts/gauntlet.sh --no-color         # plain output
#
# Exit code: 0 if all gates pass, 1 otherwise. Each gate prints
# its own pass/fail line so a single run can be inspected
# quickly.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Disable the per-(provider, model) max_tokens auto-probe so the
# gauntlet never burns the ~30 sequential HTTP probes on every cargo
# invocation it spawns. Tests that DO want the probe can override
# locally; the default is opt-out for CI.
export MOAGAN_MAX_TOKEN_AUTO=false
export MOAGAN_MAX_TOKEN_AUTO_SAVE=false

# Colour helpers
if [[ "${NO_COLOR:-}" == "1" ]] || [[ "${1:-}" == "--no-color" ]]; then
  NO_COLOR=1
fi

if [[ -t 1 ]] && [[ -z "${NO_COLOR:-}" ]]; then
  RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; BLUE=$'\033[0;34m'; BOLD=$'\033[1m'; RESET=$'\033[0m'
else
  RED=""; GREEN=""; YELLOW=""; BLUE=""; BOLD=""; RESET=""
fi

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
CURRENT_GATE=""

# Parse args
SKIP_CLIPPY=0
SKIP_SMOKE=0
FAST=0
for arg in "$@"; do
  case "$arg" in
    --fast)        FAST=1; SKIP_SMOKE=1 ;;
    --skip-smoke)  SKIP_SMOKE=1 ;;
    --skip-clippy) SKIP_CLIPPY=1 ;;
    --no-color)    NO_COLOR=1 ;;
    *) echo "Unknown arg: $arg" >&2; exit 2 ;;
  esac
done

# Helper: run a gate; record pass/fail/skip
run_gate() {
  local name="$1"
  shift
  CURRENT_GATE="$name"
  local start
  start=$(date +%s)
  if "$@"; then
    local elapsed=$(( $(date +%s) - start ))
    printf "  %s✓%s %-50s %s(%ds)%s\n" "$GREEN" "$RESET" "$name" "$BLUE" "$elapsed" "$RESET"
    PASS_COUNT=$((PASS_COUNT + 1))
  else
    local elapsed=$(( $(date +%s) - start ))
    printf "  %s✗%s %-50s %s(%ds)%s\n" "$RED" "$RESET" "$name" "$BLUE" "$elapsed" "$RESET"
    FAIL_COUNT=$((FAIL_COUNT + 1))
  fi
}

skip_gate() {
  local name="$1"
  local reason="$2"
  printf "  %s-%s %-50s %s(skipped: %s)%s\n" "$YELLOW" "$RESET" "$name" "$BLUE" "$reason" "$RESET"
  SKIP_COUNT=$((SKIP_COUNT + 1))
}

# Banner
echo
echo "${BOLD}${BLUE}moagan gauntlet${RESET} — $(date -u +'%Y-%m-%dT%H:%M:%SZ')"
echo "${BLUE}================================${RESET}"
echo

# 1. cargo fmt --all -- --check
echo "${BOLD}Formatting${RESET}"
run_gate "cargo fmt --all -- --check" bash -c "cargo fmt --all -- --check"
echo

# 2. cargo clippy --all-targets -- -D warnings
echo "${BOLD}Linting${RESET}"
if [[ "$SKIP_CLIPPY" == "1" ]]; then
  skip_gate "cargo clippy --all-targets -- -D warnings" "flag --skip-clippy"
else
  run_gate "cargo clippy --all-targets -- -D warnings" bash -c "cargo clippy --all-targets -- -D warnings"
fi
echo

# 3. cargo build
echo "${BOLD}Building${RESET}"
run_gate "cargo build" bash -c "cargo build"
echo

# 4. cargo test --all-targets
echo "${BOLD}Testing${RESET}"
run_gate "cargo test --all-targets" bash -c "cargo test --all-targets"
echo

# 5. check scripts (no-go list + forbidden crates + Anthropic SDK)
echo "${BOLD}Compliance${RESET}"
run_gate "scripts/check-no-anthropic-sdk.sh" bash -c "./scripts/check-no-anthropic-sdk.sh"
run_gate "scripts/check-no-forbidden-crates.sh" bash -c "./scripts/check-no-forbidden-crates.sh"
echo

# 6. Smoke gates
echo "${BOLD}Smoke gates${RESET}"
if [[ "$SKIP_SMOKE" == "1" ]]; then
  skip_gate "moagan run --mode fast --provider mock:mock-model" "flag --skip-smoke"
  skip_gate "moagan run --mode fast --provider minimax" "flag --skip-smoke"
else
  # Build the debug binary once so smoke gates share it.
  BIN="$ROOT/target/debug/moagan"
  if [[ ! -x "$BIN" ]]; then
    echo "${YELLOW}  !${RESET} Building debug binary first (smoke gates need it)"
    cargo build --quiet
  fi

  # Mock smoke — needs no API key, uses mock provider
  MOCK_DIR="$(mktemp -d -t moagan-mock-XXXXXX)"
  trap 'rm -rf "$MOCK_DIR"' EXIT
  run_gate "moagan run --mode fast --provider mock:mock-model --mock-dir $MOCK_DIR" \
    bash -c "$BIN run --mode fast --provider mock:mock-model --mock-dir $MOCK_DIR --non-interactive 2>&1 | tail -10"

  # MiniMax smoke — requires MINIMAX_API_KEY
  if [[ -n "${MINIMAX_API_KEY:-}" ]]; then
    MINI_DIR="$(mktemp -d -t moagan-mini-XXXXXX)"
    trap 'rm -rf "$MOCK_DIR" "$MINI_DIR"' EXIT
    run_gate "moagan run --mode fast --provider minimax:MiniMax-M3" \
      bash -c "$BIN run --mode fast --provider minimax:MiniMax-M3 --non-interactive --prompt 'simple test' 2>&1 | tail -10"
  else
    skip_gate "moagan run --mode fast --provider minimax:MiniMax-M3" "MINIMAX_API_KEY not set"
  fi
fi
echo

# 7. Git hygiene (pre-commit signing) — quick check
echo "${BOLD}Git hygiene${RESET}"
# Every commit on main is a GitHub squash-merge, signed by GitHub's web-flow
# key (B5690EEEBB952194) which is not in the local keyring by default — so
# %G? reports E (signature exists, key not in keyring). Locally-authored
# commits signed by the configured user key show G or U depending on local
# trust. This gate accepts any of G/U/E — i.e. any signature present — and
# rejects only N (no signature).
G_COUNT=$(git log --all --remotes --no-merges --format='%G?' -50 | grep -Ec '^G$|^U$|^E$' || true)
if [[ "$G_COUNT" -gt 0 ]]; then
  LAST_G=$(git log --all --remotes --no-merges --format='%H %G? %s' -50 | grep -E ' (G|U) ' | head -1)
  G_HASH=$(echo "$LAST_G" | awk '{print $1}')
  printf "  %s✓%s %-50s %s(%d signed commits in last 50, latest %s)%s\n" "$GREEN" "$RESET" "GPG-signed commits exist" "$BLUE" "$G_COUNT" "${G_HASH:0:7}" "$RESET"
  PASS_COUNT=$((PASS_COUNT + 1))
else
  printf "  %s✗%s %-50s %s(no signed commits in last 50)%s\n" "$RED" "$RESET" "GPG-signed commits exist" "$BLUE" "" "$RESET"
  FAIL_COUNT=$((FAIL_COUNT + 1))
fi
echo

# Summary
echo "${BOLD}Summary${RESET}"
printf "  %s%d passed%s, %s%d failed%s, %s%d skipped%s\n" \
  "$GREEN" "$PASS_COUNT" "$RESET" \
  "$RED" "$FAIL_COUNT" "$RESET" \
  "$YELLOW" "$SKIP_COUNT" "$RESET"
echo

if [[ "$FAIL_COUNT" -gt 0 ]]; then
  printf "%s%sFAIL%s — %d gate(s) failed\n" "$BOLD" "$RED" "$RESET" "$FAIL_COUNT"
  exit 1
else
  printf "%s%sPASS%s — gauntlet green\n" "$BOLD" "$GREEN" "$RESET"
  exit 0
fi
