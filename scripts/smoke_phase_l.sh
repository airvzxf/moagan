#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT}/target/debug/moagan"
PASS=0
FAIL=0
FAILED=()
TMP="$(mktemp)"
trap 'rm -f "${TMP}"' EXIT

run_check() {
  local name="${1}"
  shift
  if "$@" >"${TMP}" 2>&1; then
    printf 'OK: %s\n' "${name}"
    PASS=$((PASS + 1))
  else
    printf 'FAIL: %s\n' "${name}"
    sed 's/^/  /' "${TMP}"
    FAIL=$((FAIL + 1))
    FAILED+=("${name}")
  fi
}

run_check 'debug binary exists' test -x "${BIN}"
run_check 'main panic hook is installed' grep -qF -- 'std::panic::set_hook' "${ROOT}/src/main.rs"
run_check 'main panic hook uses redaction' grep -qF -- 'redact_panic_message' "${ROOT}/src/main.rs"
run_check 'main wires ReportingLayer' grep -qF -- 'ReportingLayer::new' "${ROOT}/src/main.rs"
run_check 'telemetry redaction module exists' test -f "${ROOT}/src/telemetry/redact.rs"
run_check 'ReportingLayer type exists' grep -qF -- 'pub struct ReportingLayer' "${ROOT}/src/telemetry/redact.rs"
run_check 'anthropic pattern exists' grep -qF -- 'sk-ant-[A-Za-z0-9_-]{20,}' "${ROOT}/src/redact/patterns.rs"
run_check 'openai pattern exists' grep -qF -- 'sk-[A-Za-z0-9]{20,}' "${ROOT}/src/redact/patterns.rs"
run_check 'gemini pattern exists' grep -qF -- 'AIzaSy[A-Za-z0-9_-]{20,}' "${ROOT}/src/redact/patterns.rs"
run_check 'huggingface pattern exists' grep -qF -- 'hf_[A-Za-z0-9]{20,}' "${ROOT}/src/redact/patterns.rs"
run_check 'replicate pattern exists' grep -qF -- 'r8_[A-Za-z0-9]{20,}' "${ROOT}/src/redact/patterns.rs"
run_check 'ElevenLabs pattern exists' grep -qF -- '[a-f0-9]{32}' "${ROOT}/src/redact/patterns.rs"
run_check 'SSH private key pattern exists' grep -qF -- 'PRIVATE KEY' "${ROOT}/src/redact/patterns.rs"
run_check 'PEM certificate pattern exists' grep -qF -- 'BEGIN CERTIFICATE' "${ROOT}/src/redact/patterns.rs"
run_check 'connection string pattern exists' grep -qF -- 'connection_string' "${ROOT}/src/redact/patterns.rs"
run_check 'private IP pattern exists' grep -qF -- 'private_ip' "${ROOT}/src/redact/patterns.rs"
run_check 'email pattern exists' grep -qF -- '"email"' "${ROOT}/src/redact/patterns.rs"
run_check 'credit card pattern exists' grep -qF -- 'credit_card' "${ROOT}/src/redact/patterns.rs"
run_check 'PauseReason exists' grep -qF -- 'pub enum PauseReason' "${ROOT}/src/domain.rs"
run_check 'PauseReason uses snake case' grep -qF -- 'rename_all = "snake_case"' "${ROOT}/src/domain.rs"
run_check 'CancelTier exists' grep -qF -- 'pub enum CancelTier' "${ROOT}/src/cancel.rs"
run_check 'tier cancellation method exists' grep -qF -- 'cancel_with_tier' "${ROOT}/src/cancel.rs"
run_check 'ExitCode exists' grep -qF -- 'pub enum ExitCode' "${ROOT}/src/error.rs"
run_check 'ExitCode maps I/O to 8' grep -qF -- 'IoError = 8' "${ROOT}/src/error.rs"
run_check 'Phase L integration file exists' test -f "${ROOT}/tests/integration_phase_l.rs"
run_check 'Phase L integration suite passes' cargo test --manifest-path "${ROOT}/Cargo.toml" --test integration_phase_l
run_check 'main binary panic output is redacted' bash -c "
  secret='sk-ant-abcdefghijklmnopqrst'
  output=\$(MOAGAN_PHASE_L_TEST_PANIC=\"provider key \${secret}\" \"\$1\" 2>&1)
  rc=\$?
  [[ \${rc} -ne 0 ]]
  [[ \"\${output}\" == *\"[REDACTED:anthropic_key]\"* ]]
  [[ \"\${output}\" != *\"\${secret}\"* ]]
" _ "${BIN}"
run_check 'status includes Phase L' grep -qF -- 'Sub-fase L' "${ROOT}/docs/v0.3-status.md"

printf '\nPhase L smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if [[ ${FAIL} -gt 0 ]]; then
  printf 'FAILED:\n'
  printf '  - %s\n' "${FAILED[@]}"
  exit 1
fi
exit 0
