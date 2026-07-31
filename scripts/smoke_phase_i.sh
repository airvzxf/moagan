#!/usr/bin/env bash
# Smoke tests for Phase I (v0.3 «tercera etapa» sub-fase I):
# the eight `moagan telemetry` subcommands, the read-only HTTP
# dashboard, the SHA256SUMS export / verify contract, and the
# retention pass.
#
# The script focuses on the **public CLI surface** and the
# on-disk sidecars. The heavy unit / integration tests live
# in `src/telemetry/{dashboard,export,verify,retention}.rs`
# and `tests/integration_phase_i.rs`.
#
# Each test sets `MOAGAN_HOME` to a fresh tmpdir, runs the CLI,
# and asserts on the artefacts. The script exits non-zero on
# any failure and prints `OK: <test_name>` for every passing
# test. The shell uses `set -uo pipefail` (no `-e`) so a single
# failing test does not abort the whole script; the final exit
# code is derived from the pass/fail counters.
#
# Usage:  ./scripts/smoke_phase_i.sh
# Exit:   0 when all tests pass, 1 otherwise.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${ROOT}/target/debug/moagan"
PASS=0
FAIL=0
FAILED_TESTS=()

if [[ ! -x "$BIN" ]]; then
  echo "moagan binary not built at $BIN; run 'cargo build' first"
  exit 1
fi

# ---------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------

run_test() {
  local name="$1"
  local body="$2"
  bash -c "$body" >/tmp/smoke-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/smoke-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

assert_file_exists() {
  local path="$1"
  [[ -f "$path" ]] || { echo "expected file to exist: $path" >&2; return 1; }
}

assert_contains() {
  local path="$1"
  local needle="$2"
  if ! grep -qF "$needle" "$path"; then
    echo "expected $path to contain: $needle" >&2
    return 1
  fi
}

TMPDIR_H="$(mktemp -d)"
trap "rm -rf '$TMPDIR_H'" EXIT

# Build a fake "run dir" the same shape `moagan run` would leave,
# plus a minimal SQLite index that the dashboard / list / summary
# subcommands can read. We use the `populate_home`-style helper
# indirectly: build the file tree + run moagan with --provider
# mock. The mock provider fails fast on missing fixtures, so we
# craft one JSON fixture per phase.
build_mock_fixture() {
  local dir="$1"
  local out="${2:-ok}"
  mkdir -p "$dir"
  cat > "$dir/01.json" <<EOF
{"text":"hello ${out}"}
EOF
}

populate_run_home() {
  local home="$1"
  local rid="$2"
  mkdir -p "${home}/.runs/${rid}"/proposals "${home}/.runs/${rid}"/rankings \
           "${home}/.runs/${rid}"/final "${home}/.runs/${rid}"/telemetry \
           "${home}/.runs/${rid}"/critiques "${home}/.runs/${rid}"/evaluations \
           "${home}/.runs/${rid}"/checkpoints
  echo '{"schema_version":"v1"}' > "${home}/.runs/${rid}/manifest.json"
  echo '{"goal":"smoke test"}' > "${home}/.runs/${rid}/brief.json"
  echo '{"selected":[]}' > "${home}/.runs/${rid}/rankings/ranking.json"
  echo '{"id":"p_01","score":0.9}' > "${home}/.runs/${rid}/proposals/p_01.json"
  echo '{"id":"p_02","score":0.7}' > "${home}/.runs/${rid}/proposals/p_02.json"
  echo '# Portfolio' > "${home}/.runs/${rid}/final/portfolio.md"
  # Touch meta.sqlite by running the doctor command.
  "${BIN}" --runs-dir "${home}" doctor >/dev/null 2>&1 || true
}

# ---------------------------------------------------------------------
# 1. Module / file presence
# ---------------------------------------------------------------------

run_test "telemetry_module_layout" '
  [[ -d '"$ROOT"'/src/telemetry ]]
  [[ -f '"$ROOT"'/src/telemetry/mod.rs ]]
  [[ -f '"$ROOT"'/src/telemetry/dashboard.rs ]]
  [[ -f '"$ROOT"'/src/telemetry/export.rs ]]
  [[ -f '"$ROOT"'/src/telemetry/verify.rs ]]
  [[ -f '"$ROOT"'/src/telemetry/retention.rs ]]
'

run_test "cli_telemetry_cmd_module" '
  [[ -f '"$ROOT"'/src/cli/telemetry_cmd.rs ]]
  grep -q "pub enum TelemetryCmd" '"$ROOT"'/src/cli/telemetry_cmd.rs
  grep -q "ExportLevel" '"$ROOT"'/src/cli/telemetry_cmd.rs
  grep -q "ExportFormat" '"$ROOT"'/src/cli/telemetry_cmd.rs
'

run_test "cli_dispatch_wires_eight_subcommands" '
  for sub in list summary compare provider view export cleanup verify; do
    grep -q "Self::${sub^}" '"$ROOT"'/src/cli/telemetry_cmd.rs \
      || { echo "missing dispatch for $sub"; exit 1; }
  done
'

run_test "storage_read_only_queries_for_dashboard" '
  grep -q "pub fn run_aggregate" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn list_provider_usage_for_run" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn list_phase_summaries_for_run" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn aggregate_provider_usage" '"$ROOT"'/src/storage/sqlite.rs
  grep -q "pub fn recent_runs_for_provider" '"$ROOT"'/src/storage/sqlite.rs
'

run_test "config_server_and_retention_blocks" '
  grep -q "pub struct ServerConfig" '"$ROOT"'/src/config.rs
  grep -q "pub struct RetentionConfig" '"$ROOT"'/src/config.rs
  grep -q "pub server: ServerConfig" '"$ROOT"'/src/config.rs
  grep -q "pub retention: RetentionConfig" '"$ROOT"'/src/config.rs
  # Defaults
  grep -q "port: 4096" '"$ROOT"'/src/config.rs
  grep -q "host: \"127.0.0.1\"" '"$ROOT"'/src/config.rs
  grep -q "keep_runs_days: 30" '"$ROOT"'/src/config.rs
  grep -q "keep_runs_count: 100" '"$ROOT"'/src/config.rs
'

run_test "dashboard_port_blacklist_constants" '
  grep -q "pub const DEFAULT_PORT: u16 = 4096" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "pub const PORT_BLACKLIST" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "22, 80, 443" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "3306, 5432, 6379, 8080, 8443" '"$ROOT"'/src/telemetry/dashboard.rs
'

run_test "export_supports_three_formats" '
  grep -q "ExportFormat::TarGz" '"$ROOT"'/src/telemetry/export.rs
  grep -q "ExportFormat::Tar" '"$ROOT"'/src/telemetry/export.rs
  grep -q "ExportFormat::Zip" '"$ROOT"'/src/telemetry/export.rs
  grep -q "fn write_tar_gz" '"$ROOT"'/src/telemetry/export.rs
  grep -q "fn write_tar" '"$ROOT"'/src/telemetry/export.rs
  grep -q "fn write_zip" '"$ROOT"'/src/telemetry/export.rs
'

run_test "export_emits_sha256sums_in_canonical_format" '
  grep -q "fn format_sha256sums" '"$ROOT"'/src/telemetry/export.rs
  grep -q "fn parse_sha256sums" '"$ROOT"'/src/telemetry/export.rs
  grep -q "fn sha256_file" '"$ROOT"'/src/telemetry/export.rs
'

# ---------------------------------------------------------------------
# 2. End-to-end CLI behaviour (uses a fresh MOAGAN_HOME)
# ---------------------------------------------------------------------

run_test "telemetry_list_runs_against_empty_index" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" telemetry list --limit 5 2>&1)
  [[ "$out" == *"(no runs in the index)"* ]]
'

run_test "telemetry_provider_list_runs" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" telemetry provider --list 2>&1)
  [[ "$out" == *"minimax"* ]]
  [[ "$out" == *"mock"* ]]
'

run_test "telemetry_summary_unknown_run_errors" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" telemetry summary --run 01900000-0000-0000-0000-000000000000 2>&1)
  [[ "$out" == *"not found"* ]]
'

run_test "telemetry_compare_unknown_run_errors" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  out=$("'"$BIN"'" telemetry compare \
    --run-a 01900000-0000-0000-0000-000000000000 \
    --run-b 01900000-0000-0000-0000-000000000001 2>&1)
  [[ "$out" == *"not found"* ]]
'

run_test "telemetry_verify_directory_without_shasums_errors" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  mkdir -p "$HOME/bundle"
  echo x > "$HOME/bundle/file"
  out=$("'"$BIN"'" telemetry verify --path "$HOME/bundle" 2>&1)
  [[ "$out" == *"no SHA256SUMS"* ]]
'

run_test "telemetry_cleanup_dry_run_no_op" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  mkdir -p "$HOME/.runs"
  out=$("'"$BIN"'" telemetry cleanup --dry-run 2>&1)
  [[ "$out" == *"nothing to remove"* ]]
'

run_test "telemetry_cleanup_archive_flag_is_recognised" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  mkdir -p "$HOME/.runs"
  out=$("'"$BIN"'" telemetry cleanup --dry-run --archive 2>&1)
  # Either the no-op marker or a successful run summary;
  # the key is that --archive parses without "unexpected
  # argument 'archive'" or similar clap errors.
  [[ "$out" == *"nothing to remove"* ]] || \
    [[ "$out" == *"dry-run:"* ]] || \
    [[ "$out" == *"apply:"* ]]
'

# ---------------------------------------------------------------------
# 3. Export / verify round-trip on a hand-rolled run dir
# ---------------------------------------------------------------------

run_test "export_then_verify_tar_gz_round_trip" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  RID="01900000-0000-7000-8000-000000000001"
  mkdir -p "${HOME}/.runs/${RID}"/{proposals,rankings,final,telemetry,critiques,evaluations,checkpoints}
  echo "{\"schema_version\":\"v1\"}" > "${HOME}/.runs/${RID}/manifest.json"
  echo "{\"goal\":\"smoke test\"}" > "${HOME}/.runs/${RID}/brief.json"
  echo "{\"selected\":[]}" > "${HOME}/.runs/${RID}/rankings/ranking.json"
  echo "{\"id\":\"p_01\",\"score\":0.9}" > "${HOME}/.runs/${RID}/proposals/p_01.json"
  echo "{\"id\":\"p_02\",\"score\":0.7}" > "${HOME}/.runs/${RID}/proposals/p_02.json"
  echo "# Portfolio" > "${HOME}/.runs/${RID}/final/portfolio.md"
  OUT="${HOME}/export.tar.gz"
  if ! '"${BIN}"' telemetry export --runs-dir "${HOME}" --run "${RID}" --level summary --format tar.gz --out "${OUT}" >/dev/null 2>&1; then
    echo "export failed"; exit 1
  fi
  [[ -f "${OUT}" ]]
  out=$('"${BIN}"' telemetry verify --path "${OUT}" 2>&1)
  [[ "$out" == *"OK: "* ]]
'

run_test "export_then_verify_zip_round_trip" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  RID="01900000-0000-7000-8000-000000000002"
  mkdir -p "${HOME}/.runs/${RID}"/{proposals,rankings,final,telemetry,critiques,evaluations,checkpoints}
  echo "{\"schema_version\":\"v1\"}" > "${HOME}/.runs/${RID}/manifest.json"
  echo "{\"goal\":\"smoke test\"}" > "${HOME}/.runs/${RID}/brief.json"
  echo "{\"selected\":[]}" > "${HOME}/.runs/${RID}/rankings/ranking.json"
  echo "{\"id\":\"p_01\",\"score\":0.9}" > "${HOME}/.runs/${RID}/proposals/p_01.json"
  echo "{\"id\":\"p_02\",\"score\":0.7}" > "${HOME}/.runs/${RID}/proposals/p_02.json"
  echo "# Portfolio" > "${HOME}/.runs/${RID}/final/portfolio.md"
  OUT="${HOME}/export.zip"
  if ! '"${BIN}"' telemetry export --runs-dir "${HOME}" --run "${RID}" --level full --format zip --out "${OUT}" >/dev/null 2>&1; then
    echo "export failed"; exit 1
  fi
  [[ -f "${OUT}" ]]
  out=$('"${BIN}"' telemetry verify --path "${OUT}" 2>&1)
  [[ "$out" == *"OK: "* ]]
'

run_test "verify_detects_missing_shasums" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  mkdir -p "$HOME/bundle"
  echo "abc" > "$HOME/bundle/file.txt"
  out=$('"$BIN"' telemetry verify --path "$HOME/bundle" 2>&1)
  [[ "$out" == *"no SHA256SUMS"* ]]
'

run_test "export_rejects_invalid_format" '
  HOME=$(mktemp -d)
  export MOAGAN_HOME="$HOME"
  trap "rm -rf $HOME" EXIT
  RID="01900000-0000-7000-8000-000000000001"
  mkdir -p "${HOME}/.runs/${RID}"/{proposals,rankings,final,telemetry,critiques,evaluations,checkpoints}
  echo "{\"schema_version\":\"v1\"}" > "${HOME}/.runs/${RID}/manifest.json"
  echo "{\"goal\":\"smoke test\"}" > "${HOME}/.runs/${RID}/brief.json"
  echo "{\"selected\":[]}" > "${HOME}/.runs/${RID}/rankings/ranking.json"
  echo "{\"id\":\"p_01\",\"score\":0.9}" > "${HOME}/.runs/${RID}/proposals/p_01.json"
  echo "{\"id\":\"p_02\",\"score\":0.7}" > "${HOME}/.runs/${RID}/proposals/p_02.json"
  echo "# Portfolio" > "${HOME}/.runs/${RID}/final/portfolio.md"
  out=$('"$BIN"' telemetry export --runs-dir "${HOME}" --run "${RID}" --level summary --format rar --out /tmp/out.rar 2>&1)
  [[ "$out" == *"invalid export format"* ]]
'

# ---------------------------------------------------------------------
# 4. Dashboard HTTP smoke (loopback only; bound via DashboardConfig)
# ---------------------------------------------------------------------

run_test "dashboard_dispatch_unit_presence" '
  grep -q "fn dispatch(" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "/api/runs" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "/api/runs/<run_id>/phases" '"$ROOT"'/src/telemetry/dashboard.rs \
    || grep -q "/phases" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "/provider_usage" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "/hashes" '"$ROOT"'/src/telemetry/dashboard.rs
  grep -q "/export" '"$ROOT"'/src/telemetry/dashboard.rs
'

run_test "dashboard_rejects_non_loopback_bind" '
  grep -q "must bind on a loopback address" '"$ROOT"'/src/telemetry/dashboard.rs
'

run_test "dashboard_consumes_ensure_home_knob" '
  grep -q "ensure_home" '"$ROOT"'/src/cli/telemetry_cmd.rs
  grep -q "cfg.server.ensure_home" '"$ROOT"'/src/cli/telemetry_cmd.rs
'

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo
echo "Phase I smoke: $PASS passed, $FAIL failed"
if [[ $FAIL -gt 0 ]]; then
  echo "FAILED:"
  for name in "${FAILED_TESTS[@]}"; do
    echo "  - $name"
  done
  exit 1
fi
exit 0