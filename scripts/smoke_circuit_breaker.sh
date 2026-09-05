#!/usr/bin/env bash
# Smoke checks for Track C: per-provider circuit breaker wired into
# ProviderRegistry via BreakeredProvider. Mirrors the
# phase_<letter> / smoke_<track>.sh pattern.

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

# ----------------------------------------------------------------------
# Circuit breaker API surface (catalog §D.19.5).
# ----------------------------------------------------------------------

check_circuit_breaker_public_record_success() {
    grep -qE 'pub fn record_success\(&self\)' "${ROOT}/src/llm/circuit_breaker.rs"
}

check_circuit_breaker_public_record_failure() {
    grep -qE 'pub fn record_failure\(&self\)' "${ROOT}/src/llm/circuit_breaker.rs"
}

check_circuit_breaker_public_is_open() {
    grep -qE 'pub fn is_open\(&self\) -> bool' "${ROOT}/src/llm/circuit_breaker.rs"
}

check_circuit_breaker_default_5_60_30() {
    grep -qE 'Self::new\(5, Duration::from_secs\(60\), Duration::from_secs\(30\)\)' \
        "${ROOT}/src/llm/circuit_breaker.rs"
}

# ----------------------------------------------------------------------
# Error::is_circuit_opening maps the breaker policy onto the Error
# variant set (Provider / InvalidApiKey / PlanExhausted / Timeout).
# ----------------------------------------------------------------------

check_error_is_circuit_opening_present() {
    grep -qE 'pub fn is_circuit_opening\(&self\) -> bool' "${ROOT}/src/error.rs"
}

check_error_provider_opens() {
    # Error::Provider is the 5xx upstream-error carrier; the breaker
    # must trip on it.
    awk '
        /pub fn is_circuit_opening/ {in_fn=1; next}
        in_fn && /Self::Provider/ {print; found=1; exit}
    ' "${ROOT}/src/error.rs" | grep -q "Self::Provider(_)"
}

check_error_invalid_api_key_opens() {
    awk '
        /pub fn is_circuit_opening/ {in_fn=1; next}
        in_fn && /Self::InvalidApiKey/ {print; found=1; exit}
    ' "${ROOT}/src/error.rs" | grep -q "Self::InvalidApiKey(_)"
}

check_error_schema_violation_does_not_open() {
    # SchemaViolation must NOT appear in the opener set.
    ! awk '
        /pub fn is_circuit_opening/ {in_fn=1; next}
        in_fn && /Self::SchemaViolation/ {print; exit 0}
        END {exit 1}
    ' "${ROOT}/src/error.rs"
}

check_error_cancelled_does_not_open() {
    ! awk '
        /pub fn is_circuit_opening/ {in_fn=1; next}
        in_fn && /Self::Cancelled|Self::Cancel/ {print; exit 0}
        END {exit 1}
    ' "${ROOT}/src/error.rs"
}

# ----------------------------------------------------------------------
# ProviderRegistry wraps every provider in a BreakeredProvider.
# ----------------------------------------------------------------------

check_breakered_provider_struct() {
    grep -qE 'pub struct BreakeredProvider' "${ROOT}/src/llm/provider.rs"
}

check_breakered_provider_provider_impl() {
    grep -qE 'impl Provider for BreakeredProvider' "${ROOT}/src/llm/provider.rs"
}

check_breakered_provider_send_opens_on_opening_error() {
    # The send() impl must consult Error::is_circuit_opening before
    # calling record_failure.
    grep -qE 'e\.is_circuit_opening\(\)' "${ROOT}/src/llm/provider.rs"
}

check_breakered_provider_send_fails_fast_on_open() {
    grep -qE 'if self\.breaker\.is_open\(\)' "${ROOT}/src/llm/provider.rs"
}

check_registry_insert_with_breaker() {
    # The signature may wrap onto the next line (`&mut self,`), so
    # allow the helper arg list to span multiple lines. Use a
    # multiline pcre match so we don't depend on `grep -q` emitting
    # output through a pipe.
    grep -qPzo '(?s)pub fn insert_with_breaker\([^)]*&mut self' \
        "${ROOT}/src/llm/provider.rs"
}

check_registry_breaker_accessor() {
    grep -qE 'pub fn breaker\(&self, name: &str\) -> Option<Arc<CircuitBreaker>>' \
        "${ROOT}/src/llm/provider.rs"
}

check_registry_from_config_wraps() {
    grep -qE 'insert_with_breaker\(name\.clone\(\), provider, breaker\)' \
        "${ROOT}/src/llm/provider.rs"
}

# ----------------------------------------------------------------------
# Config knob for the breaker knobs.
# ----------------------------------------------------------------------

check_circuit_breaker_config_struct() {
    grep -qE 'pub struct CircuitBreakerConfig' "${ROOT}/src/config.rs"
}

check_circuit_breaker_config_threshold_window_cooldown() {
    # All three knobs exist as fields with their defaults.
    grep -qE 'pub threshold: u32' "${ROOT}/src/config.rs"
    grep -qE 'pub window_secs: u64' "${ROOT}/src/config.rs"
    grep -qE 'pub cooldown_secs: u64' "${ROOT}/src/config.rs"
    grep -qE 'threshold: 5,' "${ROOT}/src/config.rs"
    grep -qE 'window_secs: 60,' "${ROOT}/src/config.rs"
    grep -qE 'cooldown_secs: 30,' "${ROOT}/src/config.rs"
}

# ----------------------------------------------------------------------
# CLI wires the config knob into registry_from_config.
# ----------------------------------------------------------------------

check_cli_run_passes_breaker_cfg() {
    grep -qE 'registry_from_config\(&spec_map, &cfg\.circuit_breaker\)' \
        "${ROOT}/src/cli/run.rs"
}

# ----------------------------------------------------------------------
# Tests + integration.
# ----------------------------------------------------------------------

check_unit_tests_circuit_breaker() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --lib circuit_breaker:: --quiet
}

check_unit_tests_provider_breaker() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --lib llm::provider:: --quiet
}

check_unit_tests_error_opening() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --lib error:: --quiet
}

check_unit_tests_config_breaker() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --lib config:: --quiet
}

check_integration_tests_circuit_breaker() {
    MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_circuit_breaker --quiet
}

check_no_anthropic_sdk() {
    "${ROOT}/scripts/check-no-anthropic-sdk.sh"
}

check_no_forbidden_crates() {
    "${ROOT}/scripts/check-no-forbidden-crates.sh"
}

check_clippy_clean() {
    cargo clippy --all-targets -- -D warnings >/dev/null 2>&1
}

check_fmt_clean() {
    cargo fmt --all -- --check >/dev/null 2>&1
}

run_check "circuit_breaker_public_record_success" check_circuit_breaker_public_record_success
run_check "circuit_breaker_public_record_failure" check_circuit_breaker_public_record_failure
run_check "circuit_breaker_public_is_open" check_circuit_breaker_public_is_open
run_check "circuit_breaker_default_5_60_30" check_circuit_breaker_default_5_60_30
run_check "error_is_circuit_opening_method_present" check_error_is_circuit_opening_present
run_check "error_provider_opens_breaker" check_error_provider_opens
run_check "error_invalid_api_key_opens_breaker" check_error_invalid_api_key_opens
run_check "error_schema_violation_does_not_open" check_error_schema_violation_does_not_open
run_check "error_cancelled_does_not_open" check_error_cancelled_does_not_open
run_check "breakered_provider_struct_defined" check_breakered_provider_struct
run_check "breakered_provider_implements_provider" check_breakered_provider_provider_impl
run_check "breakered_provider_send_filters_opening_errors" check_breakered_provider_send_opens_on_opening_error
run_check "breakered_provider_send_fails_fast_on_open" check_breakered_provider_send_fails_fast_on_open
run_check "registry_insert_with_breaker_helper" check_registry_insert_with_breaker
run_check "registry_breaker_accessor" check_registry_breaker_accessor
run_check "registry_from_config_wraps_with_breaker" check_registry_from_config_wraps
run_check "circuit_breaker_config_struct_with_knobs" check_circuit_breaker_config_struct
run_check "circuit_breaker_config_threshold_window_cooldown_defaults" check_circuit_breaker_config_threshold_window_cooldown
run_check "cli_run_passes_breaker_cfg" check_cli_run_passes_breaker_cfg
run_check "circuit_breaker_unit_tests" check_unit_tests_circuit_breaker
run_check "provider_breaker_unit_tests" check_unit_tests_provider_breaker
run_check "error_opening_unit_tests" check_unit_tests_error_opening
run_check "config_breaker_unit_tests" check_unit_tests_config_breaker
run_check "integration_circuit_breaker_tests" check_integration_tests_circuit_breaker
run_check "check_no_anthropic_sdk" check_no_anthropic_sdk
run_check "check_no_forbidden_crates" check_no_forbidden_crates
run_check "cargo_fmt_clean" check_fmt_clean
run_check "cargo_clippy_clean" check_clippy_clean

printf '\nCircuit breaker smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if (( FAIL > 0 )); then
    printf 'FAILED:\n'
    printf '  - %s\n' "${FAILED[@]}"
    exit 1
fi
