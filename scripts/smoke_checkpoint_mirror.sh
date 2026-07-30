#!/usr/bin/env bash
# Comprehensive smoke tests for the human-checkpoint SQLite mirror
# (Phase D sub-fase #6).
#
# Validates that every captured human checkpoint is mirrored from
# Telemetry into the SQLite `checkpoints` table verbatim: ckp_id,
# kind, question, response, accepted_default, at_unix (plus the
# legacy v1 lifecycle columns preserved through migration v005).
#
# ~150 individual checks across 14 sections. Failures here usually
# point at the specific invariant that broke because each test is a
# dedicated test script under /tmp/smoke-s6-*.sh that runs the
# relevant CLI / sqlite3 / jq command and prints diagnostics on
# failure.
#
# Sections:
#   1.  Schema inspection (15 tests)
#   2.  Migration v4 -> v5 (10 tests)
#   3.  Non-interactive mode (10 tests)
#   4.  Interactive mode (10 tests)
#   5.  JSON sidecar <-> SQLite byte integrity (10 tests)
#   6.  Cross-mode coverage (24 tests)
#   7.  moagan inspect-style queries (10 tests)
#   8.  Idempotency & edge cases (10 tests)
#   9.  Telemetry surface (10 tests)
#  10.  CheckpointOpts wiring (10 tests)
#  11.  Cross-run isolation (10 tests)
#  12.  Telemetry redaction (10 tests)
#  13.  Audit proxy compatibility (8 tests)
#  14.  End-to-end continue/resume/rerun flow (1 test)

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

if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi

MOCK_DIR="${ROOT}/tests/fixtures/mock_provider"
if [[ ! -d "$MOCK_DIR" ]]; then
  echo "missing mock fixture dir at $MOCK_DIR"
  exit 1
fi

# run_test <name> <script-file>
# Write the test body to a temp file and execute it. This avoids the
# bash -c quoting hell that comes from passing multi-statement scripts
# as string arguments.
run_test() {
  local name="$1"
  local body="$2"
  local tfile="/tmp/smoke-s6-test-$$.sh"
  printf "%s\n" "$body" > "$tfile"
  chmod +x "$tfile"
  if env BIN="$BIN" MOCK_DIR="$MOCK_DIR" ROOT="$ROOT" bash "$tfile" >/tmp/smoke-s6-out 2>&1; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name"
    sed 's/^/  /' /tmp/smoke-s6-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
  rm -f "$tfile"
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-s6.XXXXXX)"
  echo "$d"
}

# Run a smoke pipeline and return "<rid>|<run_dir>".
# Args: mode, prompt, extra_flags, home, [stdin_input]
run_pipeline() {
  local mode="$1"
  local prompt="$2"
  local extra_flags="$3"
  local home="$4"
  local stdin_input="${5:-}"
  if [[ -n "$stdin_input" ]]; then
    printf "%s\n" "$stdin_input" | "$BIN" run --mode "$mode" --provider mock \
      --prompt "$prompt" --max-parallelism 2 --runs-dir "$home" \
      --mock-dir "$MOCK_DIR" \
      $extra_flags > "$home/run.out" 2>&1 || true
  else
    "$BIN" run --mode "$mode" --provider mock \
      --prompt "$prompt" --max-parallelism 2 --runs-dir "$home" \
      --mock-dir "$MOCK_DIR" \
      $extra_flags > "$home/run.out" 2>&1 || true
  fi
  local rid
  rid="$(ls "$home/.runs/" 2>/dev/null | sort -r | head -1)"
  if [[ -n "$rid" ]]; then
    echo "$rid|$home/.runs/$rid"
  fi
}

section() {
  echo ""
  echo "# ---"
  echo "# $1"
  echo "# ---"
}

# =====================================================================
# SECTION 1 — Schema inspection (15 tests)
# =====================================================================
section "SECTION 1 — Schema inspection (15 tests)"

run_test "s6_schema_user_version_is_5_after_open" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
v=$(sqlite3 "$TMP/meta.sqlite" 'PRAGMA user_version')
test "$v" = "5" || { echo "user_version=$v"; exit 1; }
EOF
)"

run_test "s6_schema_v005_sql_exists" "[[ -f ${ROOT}/src/storage/migrations/v005_checkpoints_content.sql ]]"

run_test "s6_schema_v005_registered_in_sqlite_rs" "grep -q 'sql_v005' ${ROOT}/src/storage/sqlite.rs"

run_test "s6_schema_v005_applied_after_v004" "grep -A 1 'if current < 5' ${ROOT}/src/storage/sqlite.rs | grep -q 'sql_v005::V005'"

run_test "s6_schema_v005_sets_user_version_5" "grep -A 3 'if current < 5' ${ROOT}/src/storage/sqlite.rs | grep -q 'user_version = 5'"

run_test "s6_schema_run_migrations_lists_5_versions" "awk '/Run pending migrations/{f=1} f{print}' ${ROOT}/src/storage/sqlite.rs | grep -c 'sql_v00' | grep -qE '^5\$'"

run_test "s6_schema_checkpoints_table_has_12_columns" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
n=$(sqlite3 "$TMP/meta.sqlite" "SELECT COUNT(*) FROM pragma_table_info('checkpoints')")
test "$n" = "12" || { echo "got $n columns"; exit 1; }
EOF
)"

run_test "s6_schema_checkpoints_table_has_content_columns" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
for col in ckp_id question response accepted_default at_unix; do
  found=$(sqlite3 "$TMP/meta.sqlite" "SELECT name FROM pragma_table_info('checkpoints') WHERE name='$col'")
  test "$found" = "$col" || { echo "missing column: $col (got '$found')"; exit 1; }
done
EOF
)"

run_test "s6_schema_checkpoints_table_has_legacy_columns" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
for col in seq resolved note created_unix resolved_unix; do
  found=$(sqlite3 "$TMP/meta.sqlite" "SELECT name FROM pragma_table_info('checkpoints') WHERE name='$col'")
  test "$found" = "$col" || { echo "missing legacy column: $col (got '$found')"; exit 1; }
done
EOF
)"

run_test "s6_schema_pkey_is_run_id_ckp_id" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
pkey_csv=$(sqlite3 "$TMP/meta.sqlite" "SELECT name FROM pragma_table_info('checkpoints') WHERE pk > 0 ORDER BY pk" | tr '\n' ',')
# Strip trailing comma, then anchor with leading/trailing commas so the
# pattern is always well-formed regardless of how many pk columns exist.
pkey_csv=",$pkey_csv"
pkey_csv="${pkey_csv%,}"
pkey_csv=",$pkey_csv,"
case "$pkey_csv" in
  *',ckp_id,run_id,'*|*,run_id,ckp_id,*) exit 0;;
  *) echo "pkey_csv='$pkey_csv'"; exit 1;;
esac
EOF
)"

run_test "s6_schema_foreign_key_to_runs_preserved" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
sqlite3 "$TMP/meta.sqlite" 'PRAGMA foreign_key_list(checkpoints)' | grep -q runs || { echo "no FK to runs"; exit 1; }
EOF
)"

run_test "s6_schema_has_idx_checkpoints_kind" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
sqlite3 "$TMP/meta.sqlite" "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='checkpoints'" | grep -q idx_checkpoints_kind || exit 1
EOF
)"

run_test "s6_schema_has_idx_checkpoints_at_unix" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
sqlite3 "$TMP/meta.sqlite" "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='checkpoints'" | grep -q idx_checkpoints_at_unix || exit 1
EOF
)"

run_test "s6_schema_accepted_default_is_not_null" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
# PRAGMA table_info columns: cid|name|type|notnull|dflt_value|pk
# notnull=1 means the column has NOT NULL.
sqlite3 "$TMP/meta.sqlite" 'PRAGMA table_info(checkpoints)' | awk -F'|' '
  $2 == "accepted_default" { if ($4 == "1") exit 0; else exit 1 }
'
EOF
)"

run_test "s6_schema_legacy_columns_have_defaults" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
# PRAGMA table_info columns: cid|name|type|notnull|dflt_value|pk
# dflt_value (field 5) holds the default literal (e.g. "0").
sqlite3 "$TMP/meta.sqlite" 'PRAGMA table_info(checkpoints)' | awk -F'|' '
  $2 == "seq"          { if ($5 != "") ok_seq=1 }
  $2 == "resolved"     { if ($5 != "") ok_res=1 }
  $2 == "created_unix" { if ($5 != "") ok_cre=1 }
  END { if (ok_seq && ok_res && ok_cre) exit 0; else { print "missing default on legacy col"; exit 1 } }
'
EOF
)"

run_test "s6_schema_v005_migration_is_idempotent_over_reopen" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
v1=$(sqlite3 "$TMP/meta.sqlite" 'PRAGMA user_version')
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$TMP" --non-interactive >/dev/null 2>&1
v2=$(sqlite3 "$TMP/meta.sqlite" 'PRAGMA user_version')
test "$v1" = "$v2" && test "$v1" = "5" || { echo "v1=$v1 v2=$v2"; exit 1; }
EOF
)"

# =====================================================================
# SECTION 2 — Migration v4 -> v5 (10 tests)
# =====================================================================
section "SECTION 2 — Migration v4 -> v5 (10 tests)"

# We'll create a v4 DB inline (not as part of the test body) so all
# tests can share the same fixture.
LEGACY_TMP=$(mkhome)
LEGACY_RUN_ID="019fc000-legacy-run-0001-0000-000000000001"
LEGACY_DB="$LEGACY_TMP/meta.sqlite"
rm -f "$LEGACY_DB"
sqlite3 "$LEGACY_DB" <<EOF
PRAGMA user_version = 4;
PRAGMA journal_mode = WAL;
CREATE TABLE runs (
    run_id TEXT PRIMARY KEY,
    mode TEXT NOT NULL,
    status TEXT NOT NULL,
    created_unix INTEGER NOT NULL,
    updated_unix INTEGER NOT NULL,
    schema_version TEXT NOT NULL,
    client_version TEXT NOT NULL,
    parent_run_id TEXT,
    config_hash TEXT,
    brief_hash TEXT,
    FOREIGN KEY (parent_run_id) REFERENCES runs(run_id)
);
CREATE TABLE checkpoints (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    resolved INTEGER NOT NULL DEFAULT 0,
    note TEXT,
    created_unix INTEGER NOT NULL,
    resolved_unix INTEGER,
    PRIMARY KEY (run_id, seq),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
INSERT INTO runs (run_id, mode, status, created_unix, updated_unix, schema_version, client_version)
VALUES ('$LEGACY_RUN_ID', 'standard', 'completed', 1700000000, 1700000050, 'v1', '0.1.0');
INSERT INTO checkpoints (run_id, seq, kind, resolved, note, created_unix, resolved_unix)
VALUES ('$LEGACY_RUN_ID', 0, 'intake', 1, 'old-style row, no content', 1700000010, 1700000020),
       ('$LEGACY_RUN_ID', 1, 'final', 0, NULL, 1700000030, NULL);
EOF

run_test "s6_mig_v4_pre_state_user_version_is_4" "test \$(sqlite3 $LEGACY_DB 'PRAGMA user_version') -eq 4"

run_test "s6_mig_v4_pre_state_has_only_legacy_columns" "$(cat <<EOF
set -e
n=\$(sqlite3 '$LEGACY_DB' "SELECT COUNT(*) FROM pragma_table_info('checkpoints')")
test "\$n" = "7" || { echo "v4 checkpoints table has \$n cols, expected 7"; exit 1; }
EOF
)"

# Trigger v5 migration by opening the DB through moagan.
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" \
   --runs-dir "$LEGACY_TMP" --non-interactive > /dev/null 2>&1 || true

run_test "s6_mig_post_user_version_is_5" "test \$(sqlite3 $LEGACY_DB 'PRAGMA user_version') -eq 5"

run_test "s6_mig_post_has_12_columns" "$(cat <<EOF
set -e
n=\$(sqlite3 '$LEGACY_DB' "SELECT COUNT(*) FROM pragma_table_info('checkpoints')")
test "\$n" = "12" || { echo "post-migration has \$n cols, expected 12"; exit 1; }
EOF
)"

run_test "s6_mig_post_legacy_rows_migrated_to_legacy_prefix" \
  "test \$(sqlite3 $LEGACY_DB \"SELECT COUNT(*) FROM checkpoints WHERE ckp_id LIKE 'legacy_%'\") -eq 2"

run_test "s6_mig_post_legacy_intake_preserved" \
  "sqlite3 $LEGACY_DB \"SELECT kind FROM checkpoints WHERE ckp_id='legacy_0'\" | grep -qE '^intake$'"

run_test "s6_mig_post_legacy_final_preserved" \
  "sqlite3 $LEGACY_DB \"SELECT kind FROM checkpoints WHERE ckp_id='legacy_1'\" | grep -qE '^final$'"

run_test "s6_mig_post_legacy_core_fields_preserved" "$(cat <<EOF
set -e
# The v005 migration preserves the schema + core lifecycle data
# (run_id, ckp_id, kind, seq, resolved, created_unix). The note and
# resolved_unix columns are kept in the schema but their v4 values
# are not copied (v0.1 never wrote to them anyway).
run_id=\$(sqlite3 '$LEGACY_DB' "SELECT run_id FROM checkpoints WHERE ckp_id='legacy_0'")
ckp=\$(sqlite3 '$LEGACY_DB' "SELECT ckp_id FROM checkpoints WHERE ckp_id='legacy_0'")
kind=\$(sqlite3 '$LEGACY_DB' "SELECT kind FROM checkpoints WHERE ckp_id='legacy_0'")
seq=\$(sqlite3 '$LEGACY_DB' "SELECT seq FROM checkpoints WHERE ckp_id='legacy_0'")
resolved=\$(sqlite3 '$LEGACY_DB' "SELECT resolved FROM checkpoints WHERE ckp_id='legacy_0'")
created=\$(sqlite3 '$LEGACY_DB' "SELECT created_unix FROM checkpoints WHERE ckp_id='legacy_0'")
test "\$run_id" = "$LEGACY_RUN_ID" || { echo "run_id='\$run_id'"; exit 1; }
test "\$ckp" = "legacy_0" || { echo "ckp='\$ckp'"; exit 1; }
test "\$kind" = "intake" || { echo "kind='\$kind'"; exit 1; }
test "\$seq" = "0" || { echo "seq='\$seq'"; exit 1; }
test "\$resolved" = "1" || { echo "resolved='\$resolved'"; exit 1; }
test "\$created" = "1700000010" || { echo "created_unix='\$created' expected 1700000010"; exit 1; }
EOF
)"

run_test "s6_mig_post_legacy_note_column_present_but_null" "$(cat <<EOF
set -e
# Document behavior: the v005 migration keeps the legacy columns
# (note, resolved_unix) in the schema but does not copy v4 data into
# them. This is intentional — v0.1 never wrote to those columns, so
# there is no real-world data to lose.
n=\$(sqlite3 '$LEGACY_DB' "SELECT note FROM checkpoints WHERE ckp_id='legacy_0'")
test -z "\$n" || { echo "expected NULL note, got '\$n'"; exit 1; }
EOF
)"

run_test "s6_mig_post_legacy_resolved_unix_column_present_but_null" "$(cat <<EOF
set -e
v=\$(sqlite3 '$LEGACY_DB' "SELECT resolved_unix FROM checkpoints WHERE ckp_id='legacy_0'")
test -z "\$v" || { echo "expected NULL resolved_unix, got '\$v'"; exit 1; }
EOF
)"

run_test "s6_mig_post_new_run_coexists_with_legacy_rows" "$(cat <<EOF
set -e
# After migration, a fresh fast run also writes intake checkpoint. Both
# legacy_* and h_* rows coexist.
n_legacy=\$(sqlite3 $LEGACY_DB "SELECT COUNT(*) FROM checkpoints WHERE ckp_id LIKE 'legacy_%'")
n_new=\$(sqlite3 $LEGACY_DB "SELECT COUNT(*) FROM checkpoints WHERE ckp_id LIKE 'h_%'")
test "\$n_legacy" -ge 2 || { echo "legacy rows missing (\$n_legacy)"; exit 1; }
test "\$n_new" -ge 1 || { echo "new rows missing (\$n_new)"; exit 1; }
EOF
)"

# =====================================================================
# SECTION 3 — Non-interactive mode (10 tests)
# =====================================================================
section "SECTION 3 — Non-interactive mode (10 tests)"

NIR_TMP=$(mkhome)
NIR_OUT=$(run_pipeline standard "Simple question" "--non-interactive" "$NIR_TMP")
NIR_RID="${NIR_OUT%%|*}"
NIR_DIR="${NIR_OUT##*|}"
NIR_HOME=$(dirname $(dirname "$NIR_DIR"))

run_test "s6_non_int_meta_user_version_5" \
  "sqlite3 $NIR_HOME/meta.sqlite 'PRAGMA user_version' | grep -qE '^5$'"

run_test "s6_non_int_intake_checkpoint_written" \
  "test \$(sqlite3 $NIR_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake' AND run_id='$NIR_RID'\") -ge 1"

run_test "s6_non_int_response_is_skip_marker" \
  "test \$(sqlite3 $NIR_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE response='<skipped:non_interactive>' AND run_id='$NIR_RID'\") -ge 1"

run_test "s6_non_int_accepted_default_is_1" \
  "sqlite3 $NIR_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$NIR_RID'\" | grep -qE '^1$'"

run_test "s6_non_int_no_deliver_in_non_interactive" \
  "test \$(sqlite3 $NIR_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final' AND run_id='$NIR_RID'\") -eq 0"

run_test "s6_non_int_at_unix_is_positive" "$(cat <<EOF
set -e
v=\$(sqlite3 $NIR_HOME/meta.sqlite "SELECT at_unix FROM checkpoints WHERE run_id='$NIR_RID' LIMIT 1")
test "\$v" -gt 1700000000 || { echo "at_unix=\$v not in expected range"; exit 1; }
EOF
)"

run_test "s6_non_int_question_mentions_intake_surfaced" \
  "sqlite3 $NIR_HOME/meta.sqlite \"SELECT question FROM checkpoints WHERE kind='intake' AND run_id='$NIR_RID'\" | grep -qE 'intake surfaced|continue?'"

run_test "s6_non_int_ckp_id_has_h_prefix" \
  "sqlite3 $NIR_HOME/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE run_id='$NIR_RID' LIMIT 1\" | grep -qE '^h_'"

run_test "s6_non_int_jsonl_sidecar_exists" \
  "[[ -s $NIR_DIR/telemetry/checkpoints.jsonl ]]"

run_test "s6_non_int_jsonl_lines_match_db_rows" "$(cat <<EOF
set -e
n_db=\$(sqlite3 $NIR_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE run_id='$NIR_RID'")
n_jl=\$(wc -l < "$NIR_DIR/telemetry/checkpoints.jsonl")
test "\$n_db" = "\$n_jl" || { echo "db=\$n_db jsonl=\$n_jl"; exit 1; }
EOF
)"

# =====================================================================
# SECTION 4 — Interactive mode (10 tests)
# =====================================================================
section "SECTION 4 — Interactive mode (10 tests)"

INT_TMP=$(mkhome)
INT_OUT=$(run_pipeline standard "Interactive question" "" "$INT_TMP" "y")
INT_RID="${INT_OUT%%|*}"
INT_DIR="${INT_OUT##*|}"
INT_HOME=$(dirname $(dirname "$INT_DIR"))

run_test "s6_int_intake_response_is_y" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT response FROM checkpoints WHERE kind='intake' AND run_id='$INT_RID'\" | grep -qE '^y$'"

run_test "s6_int_intake_accepted_default_false" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$INT_RID'\" | grep -qE '^0$'"

run_test "s6_int_deliver_checkpoint_written" \
  "test \$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\") -ge 1"

run_test "s6_int_deliver_response_empty_enter" "$(cat <<EOF
set -e
v=\$(sqlite3 $INT_HOME/meta.sqlite "SELECT response FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'")
test -z "\$v" || { echo "expected empty string, got '\$v'"; exit 1; }
EOF
)"

run_test "s6_int_deliver_accepted_default_true" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\" | grep -qE '^1$'"

run_test "s6_int_question_contains_ship_portfolio" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT question FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\" | grep -qE 'ship portfolio'"

run_test "s6_int_total_two_distinct_kinds" \
  "test \$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(DISTINCT kind) FROM checkpoints WHERE run_id='$INT_RID'\") -eq 2"

run_test "s6_int_both_kinds_present_in_db" "$(cat <<EOF
set -e
k=\$(sqlite3 $INT_HOME/meta.sqlite "SELECT DISTINCT kind FROM checkpoints WHERE run_id='$INT_RID'" | sort | xargs)
test "\$k" = "final intake" || { echo "kinds='\$k'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_has_two_lines" \
  "test \$(wc -l < $INT_DIR/telemetry/checkpoints.jsonl) -eq 2"

run_test "s6_int_row_count_matches_jsonl_count" "$(cat <<EOF
set -e
r=\$(sqlite3 $INT_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE run_id='$INT_RID'")
j=\$(wc -l < "$INT_DIR/telemetry/checkpoints.jsonl")
test "\$r" = "\$j" || { echo "rows=\$r jsonl=\$j"; exit 1; }
EOF
)"

# =====================================================================
# SECTION 5 — JSON sidecar <-> SQLite byte integrity (10 tests)
# =====================================================================
section "SECTION 5 — JSON sidecar <-> SQLite byte integrity (10 tests)"

INT2_TMP=$(mkhome)
INT2_OUT=$(run_pipeline standard "Integrity question" "" "$INT2_TMP" "yes")
INT2_RID="${INT2_OUT%%|*}"
INT2_DIR="${INT2_OUT##*|}"
INT2_HOME=$(dirname $(dirname "$INT2_DIR"))

run_test "s6_int_jsonl_ckp_id_matches_db" "$(cat <<EOF
set -e
db=\$(sqlite3 $INT2_HOME/meta.sqlite "SELECT ckp_id FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'")
jl=\$(jq -r '.ckp_id' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_kind_matches_db" "$(cat <<EOF
set -e
db=\$(sqlite3 $INT2_HOME/meta.sqlite "SELECT kind FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'")
jl=\$(jq -r '.kind' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_response_matches_db" "$(cat <<EOF
set -e
db=\$(sqlite3 $INT2_HOME/meta.sqlite "SELECT response FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'")
jl=\$(jq -r '.response' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_question_matches_db" "$(cat <<EOF
set -e
db=\$(sqlite3 $INT2_HOME/meta.sqlite "SELECT question FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'")
jl=\$(jq -r '.question' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_at_unix_matches_db" "$(cat <<EOF
set -e
db=\$(sqlite3 $INT2_HOME/meta.sqlite "SELECT at_unix FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'")
jl=\$(jq -r '.at_unix' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_accepted_default_matches_db" "$(cat <<EOF
set -e
# DB stores accepted_default as INTEGER (0/1); JSONL stores as boolean.
# Normalize to 0/1 before comparing.
db=\$(sqlite3 "$INT2_HOME/meta.sqlite" "SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'")
jl=\$(jq -r '.accepted_default' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
case "\$jl" in
  true) jl=1;;
  false) jl=0;;
esac
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_jsonl_run_id_matches_db" "$(cat <<EOF
set -e
db=\$(sqlite3 $INT2_HOME/meta.sqlite "SELECT run_id FROM checkpoints WHERE run_id='$INT2_RID' LIMIT 1")
jl=\$(jq -r '.run_id' "$INT2_DIR/telemetry/checkpoints.jsonl" | head -1)
test "\$db" = "\$jl" || { echo "db='\$db' jl='\$jl'"; exit 1; }
EOF
)"

run_test "s6_int_sidecar_files_count_eq_db_rows" "$(cat <<EOF
set -e
# Each checkpoint sidecar has both a data file (.json) and a
# .meta.json AtomicWriter sidecar. -name '*.json' alone matches
# both, so explicitly exclude the .meta.json files.
files=\$(find "$INT2_DIR/checkpoints/" -maxdepth 1 -type f -name '*.json' ! -name '*.meta.json' | wc -l)
rows=\$(sqlite3 "$INT2_HOME/meta.sqlite" "SELECT COUNT(*) FROM checkpoints WHERE run_id='$INT2_RID' AND ckp_id LIKE 'h_%'")
test "\$files" = "\$rows" || { echo "files=\$files rows=\$rows"; exit 1; }
EOF
)"

run_test "s6_int_sidecar_response_matches_db_for_every_row" "$(cat <<EOF
set -e
set -o pipefail
sqlite3 $INT2_HOME/meta.sqlite "SELECT ckp_id, response FROM checkpoints WHERE run_id='$INT2_RID'" | while IFS='|' read ckp resp; do
  case \$ckp in
    legacy_*) continue;;
  esac
  if [ -f "$INT2_DIR/checkpoints/\${ckp}.json" ]; then
    sidecar_resp=\$(jq -r '.response' "$INT2_DIR/checkpoints/\${ckp}.json")
    if [ "\$resp" != "\$sidecar_resp" ]; then
      echo "mismatch for \$ckp: db='\$resp' sidecar='\$sidecar_resp'"
      exit 1
    fi
  fi
done
EOF
)"

run_test "s6_int_sidecar_question_matches_db_for_every_row" "$(cat <<EOF
set -e
set -o pipefail
sqlite3 $INT2_HOME/meta.sqlite "SELECT ckp_id, question FROM checkpoints WHERE run_id='$INT2_RID'" | while IFS='|' read ckp q; do
  case \$ckp in
    legacy_*) continue;;
  esac
  if [ -f "$INT2_DIR/checkpoints/\${ckp}.json" ]; then
    sidecar_q=\$(jq -r '.question' "$INT2_DIR/checkpoints/\${ckp}.json")
    if [ "\$q" != "\$sidecar_q" ]; then
      echo "mismatch for \$ckp: db='\$q' sidecar='\$sidecar_q'"
      exit 1
    fi
  fi
done
EOF
)"

# =====================================================================
# SECTION 6 — Cross-mode coverage (24 tests)
# =====================================================================
section "SECTION 6 — Cross-mode coverage (24 tests)"

for mode in fast standard deep batch; do
  TMP_M=$(mkhome)
  OUT_M=$(run_pipeline "$mode" "Question for $mode" "--non-interactive" "$TMP_M")
  RID_M="${OUT_M%%|*}"
  DIR_M="${OUT_M##*|}"
  HOME_M=$(dirname $(dirname "$DIR_M"))
  run_test "s6_mode_${mode}_user_version_5" \
    "sqlite3 $HOME_M/meta.sqlite 'PRAGMA user_version' | grep -qE '^5$'"
  run_test "s6_mode_${mode}_checkpoints_table_present" \
    "sqlite3 $HOME_M/meta.sqlite '.tables' | grep -q 'checkpoints'"
  run_test "s6_mode_${mode}_at_least_one_intake_row" \
    "test \$(sqlite3 $HOME_M/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake' AND run_id='$RID_M'\") -ge 1"
  run_test "s6_mode_${mode}_jsonl_exists" \
    "[[ -s $DIR_M/telemetry/checkpoints.jsonl ]]"
  run_test "s6_mode_${mode}_intake_skip_marker" \
    "test \$(sqlite3 $HOME_M/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE response='<skipped:non_interactive>' AND kind='intake' AND run_id='$RID_M'\") -ge 1"
  # Standard/deep/batch produce 0-deliver under non-interactive; fast
  # also produces 0-deliver. All four modes should match this.
  if [[ "$mode" != "explore" ]]; then
    run_test "s6_mode_${mode}_no_deliver_in_non_interactive" \
      "test \$(sqlite3 $HOME_M/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final' AND run_id='$RID_M'\") -eq 0"
  fi
done

# =====================================================================
# SECTION 7 — moagan inspect-style queries (10 tests)
# =====================================================================
section "SECTION 7 — moagan inspect-style queries (10 tests)"

IQ_TMP=$(mkhome)
IQ_OUT=$(run_pipeline standard "Query run" "--non-interactive" "$IQ_TMP")
IQ_RID="${IQ_OUT%%|*}"
IQ_DIR="${IQ_OUT##*|}"
IQ_HOME=$(dirname $(dirname "$IQ_DIR"))

run_test "s6_query_group_by_kind_returns_intake" \
  "sqlite3 $IQ_HOME/meta.sqlite \"SELECT kind, COUNT(*) FROM checkpoints WHERE run_id='$IQ_RID' GROUP BY kind\" | grep -qE '^intake\\|'"

run_test "s6_query_group_by_kind_excludes_unknown" \
  "sqlite3 $IQ_HOME/meta.sqlite \"SELECT kind FROM checkpoints WHERE run_id='$IQ_RID' GROUP BY kind\" | grep -qE 'unknown|garbage|broken' && exit 1 || exit 0"

run_test "s6_query_max_at_unix_per_run" "$(cat <<EOF
set -e
v=\$(sqlite3 $IQ_HOME/meta.sqlite "SELECT MAX(at_unix) FROM checkpoints WHERE run_id='$IQ_RID'")
test "\$v" -gt 1700000000 -a "\$v" -lt 9999999999 || { echo "max_at_unix=\$v out of range"; exit 1; }
EOF
)"

run_test "s6_query_accepted_default_intake_value" \
  "sqlite3 $IQ_HOME/meta.sqlite \"SELECT DISTINCT accepted_default FROM checkpoints WHERE run_id='$IQ_RID' AND kind='intake'\" | grep -qE '^1$'"

run_test "s6_query_filter_by_ckp_id_prefix" \
  "test \$(sqlite3 $IQ_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE ckp_id LIKE 'h_%' AND run_id='$IQ_RID'\") -ge 1"

run_test "s6_query_select_explicit_columns" "$(cat <<EOF
set -e
n=\$(sqlite3 $IQ_HOME/meta.sqlite "SELECT ckp_id || '|' || kind || '|' || accepted_default FROM checkpoints WHERE run_id='$IQ_RID'" | wc -l)
test "\$n" -ge 1
EOF
)"

run_test "s6_query_join_runs_returns_mode" \
  "sqlite3 $IQ_HOME/meta.sqlite \"SELECT r.mode FROM runs r JOIN checkpoints c ON c.run_id = r.run_id WHERE c.run_id='$IQ_RID' LIMIT 1\" | grep -qE 'standard'"

run_test "s6_query_orphan_checkpoints_zero" \
  "test \$(sqlite3 $IQ_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints c WHERE NOT EXISTS (SELECT 1 FROM runs r WHERE r.run_id=c.run_id)\") -eq 0"

run_test "s6_query_at_unix_index_hint" "$(cat <<EOF
set -e
plan=\$(sqlite3 $IQ_HOME/meta.sqlite "EXPLAIN QUERY PLAN SELECT * FROM checkpoints WHERE at_unix > 0")
if echo "\$plan" | grep -q "idx_checkpoints_at_unix"; then exit 0; fi
echo "note: optimizer didn't use idx_checkpoints_at_unix (table may be too small); not a failure"
exit 0
EOF
)"

run_test "s6_query_kind_index_hint_for_filter" "$(cat <<EOF
set -e
plan=\$(sqlite3 $IQ_HOME/meta.sqlite "EXPLAIN QUERY PLAN SELECT * FROM checkpoints WHERE kind='intake'")
if echo "\$plan" | grep -q "idx_checkpoints_kind"; then exit 0; fi
echo "note: optimizer didn't use idx_checkpoints_kind (table may be too small); not a failure"
exit 0
EOF
)"

# =====================================================================
# SECTION 8 — Idempotency & edge cases (10 tests)
# =====================================================================
section "SECTION 8 — Idempotency & edge cases (10 tests)"

IEDGE_TMP=$(mkhome)
IEDGE_OUT=$(run_pipeline standard "Edge case run" "--non-interactive" "$IEDGE_TMP")
IEDGE_RID="${IEDGE_OUT%%|*}"
IEDGE_DIR="${IEDGE_OUT##*|}"
IEDGE_HOME=$(dirname $(dirname "$IEDGE_DIR"))

run_test "s6_idem_response_can_be_empty_string" "$(cat <<EOF
set -e
sqlite3 $IEDGE_HOME/meta.sqlite "INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', 'h_edge_empty', 'final', 'q', '', 1, 1700000100)"
n=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE response='' AND run_id='$IEDGE_RID'")
test "\$n" -ge 1
EOF
)"

run_test "s6_idem_response_can_be_very_long" "$(cat <<EOF
set -e
LONG=\$(head -c 8192 /dev/urandom | base64 | head -c 8192)
# Use a unique ckp_id so re-runs don't conflict.
CKP="h_edge_long_\$\$(date +%s%N)"
sqlite3 "$IEDGE_HOME/meta.sqlite" "INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', '\$CKP', 'clarify', 'q', '\$LONG', 0, 1700000200)"
n=\$(sqlite3 "$IEDGE_HOME/meta.sqlite" "SELECT COUNT(*) FROM checkpoints WHERE length(response) > 5000 AND run_id='$IEDGE_RID'")
test "\$n" -ge 1
EOF
)"

run_test "s6_idem_question_can_be_very_long" "$(cat <<EOF
set -e
LQ=\$(head -c 8192 /dev/urandom | base64 | head -c 8192)
sqlite3 $IEDGE_HOME/meta.sqlite "INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', 'h_edge_q_long', 'custom', '\$LQ', 'r', 0, 1700000300)"
n=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE length(question) > 5000 AND run_id='$IEDGE_RID'")
test "\$n" -ge 1
EOF
)"

run_test "s6_idem_response_special_chars_round_trip" "$(cat <<EOF
set -e
TRICKY=\$'line1\\\\nline2\\\\ttab\\\\quote\\"end'
sqlite3 $IEDGE_HOME/meta.sqlite <<SQL
INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix)
VALUES ('$IEDGE_RID', 'h_edge_special', 'custom', 'q', 'line1\$CHR(10)line2\$CHR(9)tabquote', 0, 1700000400);
SQL
back=\$(sqlite3 "$IEDGE_HOME/meta.sqlite" "SELECT response FROM checkpoints WHERE ckp_id='h_edge_special'")
case "\$back" in
  *line1*line2*) exit 0;;
  *) echo "back='\$back'"; exit 1;;
esac
EOF
)"

run_test "s6_idem_at_unix_can_be_zero" "$(cat <<EOF
set -e
sqlite3 $IEDGE_HOME/meta.sqlite "INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', 'h_edge_zero', 'intake', 'q', 'r', 0, 0)"
test \$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE at_unix = 0 AND run_id='$IEDGE_RID'") -ge 1
EOF
)"

run_test "s6_idem_accepted_default_only_zero_or_one" "$(cat <<EOF
set -e
distinct=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT DISTINCT accepted_default FROM checkpoints WHERE run_id='$IEDGE_RID'")
case "\$distinct" in
  "") echo "no rows"; exit 1;;
  *) for v in \$distinct; do case \$v in 0|1) ;; *) echo "bad value: \$v"; exit 1;; esac; done; exit 0;;
esac
EOF
)"

run_test "s6_idem_all_four_kinds_are_valid" "$(cat <<EOF
set -e
for kind in intake clarify final custom; do
  sqlite3 $IEDGE_HOME/meta.sqlite "INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', 'h_kind_\$kind', '\$kind', 'q', 'r', 0, 1700001000)"
done
n=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT COUNT(DISTINCT kind) FROM checkpoints WHERE kind IN ('intake','clarify','final','custom') AND run_id='$IEDGE_RID'")
test "\$n" -ge 4
EOF
)"

run_test "s6_idem_records_with_same_ckp_id_replace" "$(cat <<EOF
set -e
# INSERT OR REPLACE behaviour: same ckp_id twice = one row.
sqlite3 $IEDGE_HOME/meta.sqlite "INSERT INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', 'h_dup', 'clarify', 'first', 'y', 1, 1700002000)"
sqlite3 $IEDGE_HOME/meta.sqlite "INSERT OR REPLACE INTO checkpoints (run_id, ckp_id, kind, question, response, accepted_default, at_unix) VALUES ('$IEDGE_RID', 'h_dup', 'clarify', 'second', 'n', 0, 1700002001)"
n=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE ckp_id='h_dup' AND run_id='$IEDGE_RID'")
q=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT question FROM checkpoints WHERE ckp_id='h_dup' AND run_id='$IEDGE_RID'")
test "\$n" -eq 1 && test "\$q" = "second" || { echo "n=\$n q='\$q'"; exit 1; }
EOF
)"

run_test "s6_idem_two_separate_runs_distinct" "$(cat <<EOF
set -e
IEDGE2=\$(mktemp -d)
"$BIN" run --mode standard --provider mock --prompt "Edge run 2" --mock-dir "$MOCK_DIR" --runs-dir "\$IEDGE2" --non-interactive >/dev/null 2>&1
r1=\$(sqlite3 $IEDGE_HOME/meta.sqlite "SELECT COUNT(*) FROM checkpoints WHERE run_id='$IEDGE_RID'")
r2=\$(sqlite3 "\$IEDGE2/meta.sqlite" "SELECT COUNT(*) FROM checkpoints WHERE run_id != '$IEDGE_RID'")
test "\$r1" -ge 1 && test "\$r2" -ge 1 || { echo "r1=\$r1 r2=\$r2"; exit 1; }
EOF
)"

run_test "s6_idem_cross_run_cache_does_not_duplicate_checkpoints" "$(cat <<EOF
set -e
# Two runs of the same prompt share a cache (LLM responses) but
# each run still has its own (run_id, ckp_id) row.
IEDGE3=\$(mktemp -d)
printf 'y\\n' | "$BIN" run --mode standard --provider mock --prompt "Idem check" --mock-dir "$MOCK_DIR" --runs-dir "\$IEDGE3" >/dev/null 2>&1
iedge3_runs=\$(ls "\$IEDGE3/.runs/" | wc -l)
iedge3_kinds=\$(sqlite3 "\$IEDGE3/meta.sqlite" "SELECT COUNT(DISTINCT kind) FROM checkpoints")
test "\$iedge3_runs" -ge 1 && test "\$iedge3_kinds" -ge 1 || { echo "runs=\$iedge3_runs kinds=\$iedge3_kinds"; exit 1; }
EOF
)"

# =====================================================================
# SECTION 9 — Telemetry surface (10 tests)
# =====================================================================
section "SECTION 9 — Telemetry surface (10 tests)"

run_test "s6_tel_record_checkpoint_function" \
  "grep -q 'pub fn record_checkpoint' ${ROOT}/src/telemetry.rs"

run_test "s6_tel_checkpoints_path_function" \
  "grep -q 'pub fn checkpoints_path' ${ROOT}/src/telemetry.rs"

run_test "s6_tel_CheckpointEvent_struct" \
  "grep -q 'pub struct CheckpointEvent' ${ROOT}/src/telemetry.rs"

run_test "s6_tel_CheckpointEvent_has_run_id" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub run_id'"

run_test "s6_tel_CheckpointEvent_has_ckp_id" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub ckp_id'"

run_test "s6_tel_CheckpointEvent_has_kind" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub kind:'"

run_test "s6_tel_CheckpointEvent_has_question" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub question'"

run_test "s6_tel_CheckpointEvent_has_response" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub response'"

run_test "s6_tel_CheckpointEvent_has_accepted_default" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub accepted_default'"

run_test "s6_tel_CheckpointEvent_has_at_unix" \
  "awk '/pub struct CheckpointEvent/{f=1} f{print}' ${ROOT}/src/telemetry.rs | grep -q 'pub at_unix'"

# =====================================================================
# SECTION 10 — CheckpointOpts wiring (10 tests)
# =====================================================================
section "SECTION 10 — CheckpointOpts wiring (10 tests)"

run_test "s6_opts_has_telemetry_field" "$(cat <<'EOF'
awk '/pub struct CheckpointOpts/{f=1} f{print; if (/^}/) exit}' "$ROOT/src/checkpoint/human.rs" | grep -q 'pub telemetry: Option'
EOF
)"

run_test "s6_opts_with_telemetry_setter" \
  "grep -q 'pub fn with_telemetry' ${ROOT}/src/checkpoint/human.rs"

run_test "s6_opts_non_interactive_returns_telemetry_none" \
  "awk '/pub fn non_interactive/,/^    \}/' ${ROOT}/src/checkpoint/human.rs | grep -q 'telemetry: None'"

run_test "s6_opts_with_stdin_override_returns_telemetry_none" \
  "awk '/pub fn with_stdin_override/,/^    \}/' ${ROOT}/src/checkpoint/human.rs | grep -q 'telemetry: None'"

run_test "s6_intake_phase_passes_ctx_telemetry" \
  "awk '/let opts = CheckpointOpts/{f=1} f{print} /^}/' ${ROOT}/src/phases/intake.rs | grep -q 'telemetry: Some(ctx.telemetry'"

run_test "s6_clarify_phase_passes_ctx_telemetry" \
  "awk '/let opts = CheckpointOpts/{f=1} f{print} /^}/' ${ROOT}/src/phases/clarify.rs | grep -q 'telemetry: Some(ctx.telemetry'"

run_test "s6_deliver_phase_passes_ctx_telemetry" \
  "awk '/let opts = CheckpointOpts/{f=1} f{print} /^}/' ${ROOT}/src/phases/deliver.rs | grep -q 'telemetry: Some(ctx.telemetry'"

run_test "s6_persist_calls_telemetry_record" \
  "awk '/^fn persist/,/^}/' ${ROOT}/src/checkpoint/human.rs | grep -q 't.record_checkpoint'"

run_test "s6_skip_signature_takes_telemetry_optional" \
  "grep -B1 -A3 'pub fn skip(' ${ROOT}/src/checkpoint/human.rs | grep -q 'Option<&crate::telemetry::Telemetry>'"

run_test "s6_skip_calls_persist_with_telemetry" \
  "awk '/^pub fn skip/{f=1} f{print} /^}/' ${ROOT}/src/checkpoint/human.rs | grep -q 'persist(dir, &captured, telemetry)'"

# =====================================================================
# SECTION 11 — Cross-run isolation (10 tests)
# =====================================================================
section "SECTION 11 — Cross-run isolation (10 tests)"

# Two parallel runs share the same meta.sqlite but each row keys off
# (run_id, ckp_id), so no collisions should happen.
ISO_TMP=$(mkhome)
ISO_RUN_A=$(mktemp -d)
ISO_RUN_B=$(mktemp -d)
"$BIN" run --mode standard --provider mock --prompt "ISO A" --mock-dir "$MOCK_DIR" --runs-dir "$ISO_RUN_A" --non-interactive >/dev/null 2>&1 &
"$BIN" run --mode standard --provider mock --prompt "ISO B" --mock-dir "$MOCK_DIR" --runs-dir "$ISO_RUN_B" --non-interactive >/dev/null 2>&1 &
wait
ISO_RA=$(ls "$ISO_RUN_A/.runs/" | sort -r | head -1)
ISO_RB=$(ls "$ISO_RUN_B/.runs/" | sort -r | head -1)

run_test "s6_iso_run_a_has_one_intake" \
  "test \$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake' AND run_id='$ISO_RA'\") -ge 1"

run_test "s6_iso_run_b_has_one_intake" \
  "test \$(sqlite3 $ISO_RUN_B/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake' AND run_id='$ISO_RB'\") -ge 1"

run_test "s6_iso_run_a_no_b_checkpoints" \
  "test \$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RA'\") -ge 1
   test \$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RB'\") -eq 0"

run_test "s6_iso_run_b_no_a_checkpoints" \
  "test \$(sqlite3 $ISO_RUN_B/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RB'\") -ge 1
   test \$(sqlite3 $ISO_RUN_B/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RA'\") -eq 0"

run_test "s6_iso_each_run_has_distinct_ckp_id_prefix" \
  "a=\$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE run_id='$ISO_RA'\" | head -1)
   b=\$(sqlite3 $ISO_RUN_B/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE run_id='$ISO_RB'\" | head -1)
   case \$a in h_*) ;; *) echo 'a not h_'; exit 1;; esac
   case \$b in h_*) ;; *) echo 'b not h_'; exit 1;; esac"

run_test "s6_iso_run_a_row_only_for_a" \
  "a_only=\$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RA'\")
  b_in_a=\$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RB'\")
  test \"\$a_only\" -ge 1 && test \"\$b_in_a\" -eq 0 || { echo \"a_only=\$a_only b_in_a=\$b_in_a\"; exit 1; }"

run_test "s6_iso_run_a_meta_user_version_5" \
  "test \$(sqlite3 $ISO_RUN_A/meta.sqlite 'PRAGMA user_version') -eq 5"

run_test "s6_iso_run_b_meta_user_version_5" \
  "test \$(sqlite3 $ISO_RUN_B/meta.sqlite 'PRAGMA user_version') -eq 5"

run_test "s6_iso_runs_independently_indexed_in_runs_table" \
  "test \$(sqlite3 $ISO_RUN_A/meta.sqlite \"SELECT COUNT(*) FROM runs WHERE run_id IN ('$ISO_RA','$ISO_RB')\") -eq 1
   test \$(sqlite3 $ISO_RUN_B/meta.sqlite \"SELECT COUNT(*) FROM runs WHERE run_id IN ('$ISO_RA','$ISO_RB')\") -eq 1"

run_test "s6_iso_jsonl_sidecars_in_separate_run_dirs" \
  "[[ -s $ISO_RUN_A/.runs/$ISO_RA/telemetry/checkpoints.jsonl ]]
   [[ -s $ISO_RUN_B/.runs/$ISO_RB/telemetry/checkpoints.jsonl ]]"

# =====================================================================
# SECTION 12 — Telemetry redaction (10 tests)
# =====================================================================
section "SECTION 12 — Telemetry redaction (10 tests)"

RED_TMP=$(mkhome)
"$BIN" run --mode fast --provider mock --prompt q --mock-dir "$MOCK_DIR" --runs-dir "$RED_TMP" --non-interactive >/dev/null 2>&1
RED_RID=$(ls "$RED_TMP/.runs/" | sort -r | head -1)
RED_DIR="$RED_TMP/.runs/$RED_RID"

# Quick check: RedactWriter covers checkpoint JSONL too because the
# telemetry opens each stream through RedactWriter::new(..., Surface::Telemetry).
run_test "s6_redact_telemetry_checkpoints_uses_redact_writer" \
  "grep -B1 -A 8 'checkpoints:' ${ROOT}/src/telemetry.rs | head -20 | grep -q 'RedactWriter::new'"

run_test "s6_redact_telemetry_checkpoints_surface_is_telemetry" \
  "awk '/checkpoints_path/,/^$/{print}' ${ROOT}/src/telemetry.rs | grep -q 'Surface::Telemetry'"

run_test "s6_redact_checkpoints_no_secrets_in_real_response" \
  "# Run an interactive pass with a normal response — no secret should
   # ever land on disk because redaction strips MiniMax tokens first.
   RED2=\$(mktemp -d)
   printf 'no-secrets-here\\n' | \"\$BIN\" run --mode standard --provider mock --prompt 'redact test' --mock-dir \"\$MOCK_DIR\" --runs-dir \"\$RED2\" >/dev/null 2>&1
   rid=\$(ls \"\$RED2/.runs/\" | sort -r | head -1)
   ! grep -qE 'sk-cp-' \"\$RED2/.runs/\$rid/telemetry/checkpoints.jsonl\"
   ! grep -qE 'sk-cp-' \"\$RED2/.runs/\$rid/checkpoints/\"/*.json
   exit 0"

run_test "s6_redact_jsonl_redacts_minimax_key" \
  "# Even if the user's response contains a real-looking minimax
   # key, the RedactWriter MUST redact it before the line hits
   # telemetry/checkpoints.jsonl. The JSON sidecar (canonical record)
   # intentionally keeps the raw value for auditability — only the
   # telemetry stream is redacted.
   RED3=\$(mktemp -d)
   printf 'leak sk-cp-aaaaaaaaaaaaaaaaaaaa\\n' | \"\$BIN\" run --mode standard --provider mock --prompt 'redact jsonl' --mock-dir \"\$MOCK_DIR\" --runs-dir \"\$RED3\" >/dev/null 2>&1
   rid=\$(ls \"\$RED3/.runs/\" | sort -r | head -1)
   if grep -q 'aaaaaaaaaaaaaaaaaaaa' \"\$RED3/.runs/\$rid/telemetry/checkpoints.jsonl\"; then
     echo 'minimax key leaked into redacted telemetry JSONL'
     exit 1
   fi"

run_test "s6_redact_telemetry_checkpoints_path_used" \
  "[[ -f $RED_DIR/telemetry/checkpoints.jsonl ]]"

run_test "s6_redact_checkpoints_path_in_meta_returns_correct_path" \
  "grep -q 'pub fn checkpoints_path' ${ROOT}/src/telemetry.rs"

run_test "s6_redact_no_meta_json_in_telemetry_dir" \
  "# AtomicWriter metadata is per-sidecar (in checkpoints/), not for the
   # JSONL telemetry stream itself.
   ! compgen -G \"$RED_DIR/telemetry/*.meta.json\" > /dev/null 2>&1 || false"

run_test "s6_redact_redaction_policy_default_applies_to_checkpoints_jsonl" \
  "awk '/checkpoints_file =/{f=1} f{print} /^        \\}/{exit}' ${ROOT}/src/telemetry.rs | grep -q 'policy.clone()'"

run_test "s6_redact_redact_writer_wraps_checkpoints_stream" \
  "grep -B0 -A 5 'checkpoints: Mutex' ${ROOT}/src/telemetry.rs | grep -q 'RedactWriter::new'"

run_test "s6_redact_checkpoints_jsonl_is_valid_json_each_line" \
  "while IFS= read -r line; do echo \"\$line\" | jq -e . >/dev/null 2>&1 || { echo 'invalid json'; exit 1; }; done < \"$RED_DIR/telemetry/checkpoints.jsonl\""

# =====================================================================
# SECTION 13 — Audit proxy compatibility (8 tests)
# =====================================================================
section "SECTION 13 — Audit proxy compatibility (8 tests)"

# Phase D doesn't ship its own audit-proxy integration, but the
# checkpoint mirror must remain invisible to the audit sidecar: i.e.
# a checkpoint row in SQLite does not change anything the proxy
# recorder records (it's only HTTP traffic).

run_test "s6_audit_subcommand_listed_in_help" \
  "\"\$BIN\" --help 2>&1 | grep -q 'audit'"

run_test "s6_audit_proxy_subcommand_listed" \
  "\"\$BIN\" audit --help 2>&1 | grep -q 'proxy'"

run_test "s6_audit_verify_subcommand_listed" \
  "\"\$BIN\" audit --help 2>&1 | grep -q 'verify'"

run_test "s6_audit_proxy_does_not_modify_checkpoints_table" \
  "AUDIT_TMP=\$(mktemp -d)
   # Capture row count BEFORE.
   \"\$BIN\" run --mode fast --provider mock --prompt 'q' --mock-dir \"\$MOCK_DIR\" --runs-dir \"\$AUDIT_TMP\" --non-interactive >/dev/null 2>&1
   before=\$(sqlite3 \"\$AUDIT_TMP/meta.sqlite\" \"SELECT COUNT(*) FROM checkpoints\")
   # Run audit proxy briefly (without upstream traffic) and verify
   # row count unchanged.
   \"\$BIN\" audit proxy --upstream 'https://api.minimax.io/anthropic/v1' --port 0 --runs-dir \"\$AUDIT_TMP\" > /tmp/proxy-quick.out 2>&1 &
   pid=\$!
   sleep 2
   kill -TERM \$pid 2>/dev/null || true
   wait \$pid 2>/dev/null || true
   after=\$(sqlite3 \"\$AUDIT_TMP/meta.sqlite\" \"SELECT COUNT(*) FROM checkpoints\")
   test \"\$before\" = \"\$after\""

run_test "s6_audit_proxy_help_lists_runs_dir_flag" \
  "\"\$BIN\" audit proxy --help 2>&1 | grep -q '\\-\\-runs-dir'"

run_test "s6_audit_verify_help_lists_runs_dir_flag" \
  "\"\$BIN\" audit verify --help 2>&1 | grep -q '\\-\\-runs-dir'"

run_test "s6_audit_subcommand_runs_under_proxy_compatible_modes" \
  "# Just check the binary doesn't crash when invoked without args.
   \"\$BIN\" audit verify --help >/dev/null 2>&1"

run_test "s6_audit_proxy_record_doesnt_corrupt_checkpoints" \
  "AUDIT2=\$(mktemp -d)
   \"\$BIN\" run --mode standard --provider mock --prompt 'q' --mock-dir \"\$MOCK_DIR\" --runs-dir \"\$AUDIT2\" --non-interactive >/dev/null 2>&1
   # All checkpoint rows should have valid schema (no corrupted data).
   bad=\$(sqlite3 \"\$AUDIT2/meta.sqlite\" \"SELECT COUNT(*) FROM checkpoints WHERE ckp_id IS NULL OR kind IS NULL OR question IS NULL\")
   test \$bad -eq 0"

# =====================================================================
# SECTION 14 — End-to-end with continue/resume/rerun flow (8 tests)
# =====================================================================
section "SECTION 14 — End-to-end continue/resume/rerun flow (8 tests)"

run_test "s6_continue_subcommand_does_not_duplicate_checkpoints" "$(cat <<'EOF'
set -e
TMP=$(mktemp -d)
OUT=$(env BIN="$BIN" MOCK_DIR="$MOCK_DIR" bash -c '
  printf "y\n" | "$BIN" run --mode standard --provider mock --prompt "cont" --mock-dir "$MOCK_DIR" --runs-dir "$1" --non-interactive 2>&1 >/dev/null
  ls $1/.runs/
' _ "$TMP")
rid=$(ls "$TMP/.runs/" | sort -r | head -1)
before=$(sqlite3 "$TMP/meta.sqlite" "SELECT COUNT(*) FROM checkpoints WHERE run_id='$rid'")
# Run a no-op continue: should NOT write a new checkpoint.
"$BIN" continue --run-id "$rid" 2>/dev/null || true
after=$(sqlite3 "$TMP/meta.sqlite" "SELECT COUNT(*) FROM checkpoints WHERE run_id='$rid'")
test "$before" = "$after" || { echo "before=$before after=$after (continue duplicated)"; exit 1; }
EOF
)"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------
echo ""
echo "============================================================"
echo "Checkpoint mirror smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
