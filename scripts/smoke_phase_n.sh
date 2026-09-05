#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp)"
PASS=0
FAIL=0
FAILED=()
trap 'rm -f "${TMP}"' EXIT

run_check() {
    local check_name="$1"
    local function_name="$2"
    if "${function_name}" >"${TMP}" 2>&1; then
        printf 'OK: %s\n' "${check_name}"
        PASS=$((PASS + 1))
    else
        printf 'FAIL: %s\n' "${check_name}"
        sed 's/^/  /' "${TMP}"
        FAIL=$((FAIL + 1))
        FAILED+=("${check_name}")
    fi
}

check_module_layout() {
    [[ -f "${ROOT}/src/sandbox/process.rs" ]]
    [[ -f "${ROOT}/src/sandbox/mod.rs" ]]
    [[ -f "${ROOT}/tests/integration_phase_n.rs" ]]
}

check_default_cap() {
    grep -qF 'pub const DEFAULT_OUTPUT_CAP_BYTES: usize = 64 * 1024;' "${ROOT}/src/sandbox/process.rs"
}

check_truncation_error() {
    grep -qF 'OutputTruncated' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'start_kill' "${ROOT}/src/sandbox/process.rs"
}

check_bounded_reader() {
    grep -qF 'async fn read_stream' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'max_output_bytes' "${ROOT}/src/sandbox/process.rs"
}

check_command_config_struct() {
    grep -qF 'pub struct CommandConfig' "${ROOT}/src/sandbox/process.rs"
    for field in name binary max_args max_arg_len max_output_bytes timeout_secs allow_network; do
        grep -qF "pub ${field}:" "${ROOT}/src/sandbox/process.rs"
    done
}

check_command_config_entries() {
    for name in rust python typescript sql; do
        grep -qF "name: \"${name}\"" "${ROOT}/src/sandbox/process.rs"
    done
    grep -qF 'pub static COMMAND_CONFIGS' "${ROOT}/src/sandbox/process.rs"
}

check_config_lookup() {
    grep -qF 'pub fn config_for(name: &str)' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'config.name == name' "${ROOT}/src/sandbox/process.rs"
}

check_argv_redaction() {
    grep -qF 'pub fn strip_secrets(args: &[String]) -> Vec<String>' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'strip_secrets(&raw_args)' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'crate::redact' "${ROOT}/src/sandbox/process.rs"
}

check_secret_prefixes() {
    for prefix in 'sk-cp-' 'sk-ant-' 'AIzaSy' 'hf_' 'r8_' 'ghp_' 'xoxb-' 'Bearer '; do
        grep -qF "starts_with(\"${prefix}\")" "${ROOT}/src/sandbox/process.rs"
    done
}

check_binary_preflight() {
    grep -qF 'pub fn verify_binary_exists(binary: &str)' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'verify_binary_exists(cmd)' "${ROOT}/src/sandbox/process.rs"
}

check_binary_error() {
    grep -qF 'BinaryNotFound(String)' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'SandboxError::BinaryNotFound' "${ROOT}/src/sandbox/process.rs"
}

check_public_exports() {
    grep -qF 'SandboxError' "${ROOT}/src/sandbox/mod.rs"
    grep -qF 'strip_secrets' "${ROOT}/src/sandbox/mod.rs"
    grep -qF 'verify_binary_exists' "${ROOT}/src/sandbox/mod.rs"
}

check_unit_tests() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --lib sandbox::process
}

check_integration_tests() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_phase_n
}

run_check "sandbox_module_layout" check_module_layout
run_check "default_output_cap" check_default_cap
run_check "output_truncation_kills_child" check_truncation_error
run_check "bounded_output_reader" check_bounded_reader
run_check "command_config_struct" check_command_config_struct
run_check "command_config_entries" check_command_config_entries
run_check "command_config_lookup" check_config_lookup
run_check "argv_redaction_wiring" check_argv_redaction
run_check "secret_prefix_coverage" check_secret_prefixes
run_check "binary_preflight_wiring" check_binary_preflight
run_check "binary_not_found_error" check_binary_error
run_check "sandbox_public_exports" check_public_exports
run_check "sandbox_unit_tests" check_unit_tests
run_check "phase_n_integration_tests" check_integration_tests

printf '\nPhase N smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if (( FAIL > 0 )); then
    printf 'FAILED:\n'
    printf '  - %s\n' "${FAILED[@]}"
    exit 1
fi
