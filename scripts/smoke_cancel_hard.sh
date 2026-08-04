#!/usr/bin/env bash
# Smoke checks for Track B: CancelTier::Hard kills the sandbox's
# process group via SIGTERM → SIGKILL. Mirrors the phase_N / phase_O
# smoke-script pattern.

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
# Source-level: Cancel carries a pgid registry and a tier-aware path.
# ----------------------------------------------------------------------

check_cancel_inner_struct() {
    grep -qF 'struct Inner' "${ROOT}/src/cancel.rs"
    grep -qF 'child_pgids: Arc<parking_lot::Mutex<HashSet<i32>>>' "${ROOT}/src/cancel.rs"
}

check_cancel_inner_renamed_token() {
    # The token field is now `token` (inside Inner), not `inner`.
    ! grep -qE '^\s*inner:\s*Arc<TkToken>' "${ROOT}/src/cancel.rs"
}

check_register_unregister() {
    grep -qF 'pub fn register_child(&self, pgid: i32)' "${ROOT}/src/cancel.rs"
    grep -qF 'pub fn unregister_child(&self, pgid: i32)' "${ROOT}/src/cancel.rs"
}

check_cancel_with_tier_dispatches() {
    grep -qF 'pub fn cancel_with_tier(&self, reason: CancelReason, tier: CancelTier)' "${ROOT}/src/cancel.rs"
    grep -qF 'matches!(tier, CancelTier::Hard)' "${ROOT}/src/cancel.rs"
    grep -qF 'libc::killpg' "${ROOT}/src/cancel.rs"
}

check_grace_constant() {
    grep -qF 'pub const HARD_KILL_GRACE: Duration = Duration::from_secs(2);' "${ROOT}/src/cancel.rs"
    grep -qF 'tokio::time::sleep(HARD_KILL_GRACE)' "${ROOT}/src/cancel.rs"
}

check_libc_dep_added() {
    grep -qE '^libc\s*=' "${ROOT}/Cargo.toml"
}

# ----------------------------------------------------------------------
# Sandbox: pre_exec setpgid + with_cancel builder + pgid plumbing.
# ----------------------------------------------------------------------

check_sandbox_cancel_field() {
    grep -qF 'cancel: Option<Cancel>' "${ROOT}/src/sandbox/process.rs"
}

check_sandbox_with_cancel_builder() {
    grep -qF 'pub fn with_cancel(mut self, cancel: Cancel) -> Self' "${ROOT}/src/sandbox/process.rs"
}

check_sandbox_pre_exec_setpgid() {
    grep -qF 'command.pre_exec' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'libc::setpgid(0, 0)' "${ROOT}/src/sandbox/process.rs"
    grep -qF '#[cfg(unix)]' "${ROOT}/src/sandbox/process.rs"
}

check_sandbox_register_unregister_pgid() {
    grep -qF 'cancel.register_child(pgid)' "${ROOT}/src/sandbox/process.rs"
    grep -qF 'cancel.unregister_child(pgid)' "${ROOT}/src/sandbox/process.rs"
}

# ----------------------------------------------------------------------
# ValidatePhase wires ctx.cancel into the sandbox.
# ----------------------------------------------------------------------

check_validate_wires_cancel() {
    grep -qF '.with_cancel(ctx.cancel().clone())' "${ROOT}/src/phases/validate.rs"
}

# ----------------------------------------------------------------------
# Tests + integration.
# ----------------------------------------------------------------------

check_unit_tests_cancel() {
    cargo test --lib cancel:: --quiet
}

check_unit_tests_sandbox() {
    cargo test --lib sandbox::process:: --quiet
}

check_integration_tests() {
    cargo test --test integration_cancel_hard_kill --quiet
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

run_check "cancel_inner_struct_with_pgid_registry" check_cancel_inner_struct
run_check "cancel_token_field_moved_into_inner" check_cancel_inner_renamed_token
run_check "cancel_register_unregister_methods" check_register_unregister
run_check "cancel_with_tier_hard_killpg_dispatch" check_cancel_with_tier_dispatches
run_check "cancel_hard_kill_grace_constant" check_grace_constant
run_check "libc_dependency_added" check_libc_dep_added
run_check "sandbox_cancel_field_optional" check_sandbox_cancel_field
run_check "sandbox_with_cancel_builder" check_sandbox_with_cancel_builder
run_check "sandbox_pre_exec_setpgid" check_sandbox_pre_exec_setpgid
run_check "sandbox_register_unregister_pgid" check_sandbox_register_unregister_pgid
run_check "validate_phase_wires_ctx_cancel" check_validate_wires_cancel
run_check "cancel_unit_tests" check_unit_tests_cancel
run_check "sandbox_unit_tests" check_unit_tests_sandbox
run_check "integration_cancel_hard_kill_tests" check_integration_tests
run_check "check_no_anthropic_sdk" check_no_anthropic_sdk
run_check "check_no_forbidden_crates" check_no_forbidden_crates
run_check "cargo_fmt_clean" check_fmt_clean
run_check "cargo_clippy_clean" check_clippy_clean

printf '\nCancel-hard smoke: %d passed, %d failed\n' "${PASS}" "${FAIL}"
if (( FAIL > 0 )); then
    printf 'FAILED:\n'
    printf '  - %s\n' "${FAILED[@]}"
    exit 1
fi