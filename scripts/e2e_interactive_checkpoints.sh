#!/usr/bin/env bash
# End-to-end tests that exercise `moagan run` against the mock LLM
# provider and verify the human-checkpoint capture pipeline.
#
# These were extracted from `smoke_human_checkpoint.sh` §H8 and
# `smoke_checkpoint_mirror.sh` §3 / §4 / §5 / §6 / §11 because they
# were the only sections across both files that:
#
#   1. launched the binary with `run_pipeline` (i.e. a real mock
#      pipeline, ~3-10 s each), and
#   2. had a coherent e2e shape (one mode, one brief, verify the
#      checkpoint rows in `meta.sqlite`).
#
# Putting them in a single script makes it cheap to run them
# together when validating refactors to `src/checkpoint/human.rs`
# or `src/telemetry.rs`, without paying the cost on every static
# grep in the smoke suite.
#
# Coverage:
#   A. Non-interactive mode capture (10 tests, smoke_checkpoint_mirror.sh §3)
#   B. Interactive mode via stdin (10 tests, smoke_checkpoint_mirror.sh §4)
#   C. JSON sidecar <-> SQLite byte integrity (10 tests,
#      smoke_checkpoint_mirror.sh §5)
#   D. Cross-mode coverage (24 tests, smoke_checkpoint_mirror.sh §6)
#   E. Cross-run isolation with parallel binary runs (10 tests,
#      smoke_checkpoint_mirror.sh §11)
#   F. Interactive end-to-end (5 tests, smoke_human_checkpoint.sh §H8)
#
# Companion static suites (run in <5 s):
#   scripts/smoke_human_checkpoint.sh     (47 tests — module shape)
#   scripts/smoke_checkpoint_mirror.sh    (60 tests — schema / queries / DB)

set -o pipefail

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
[[ -d "$MOCK_DIR" ]] || { echo "missing mock fixture at $MOCK_DIR"; exit 1; }

run_test() {
  local name="$1"
  local body="$2"
  # We pass every variable the body might reference because bash -c
  # starts a fresh shell that does not see the parent's lexical vars.
  # Any parent-side scalar used in `body` MUST be listed here.
  env ROOT="$ROOT" BIN="$BIN" MOCK_DIR="$MOCK_DIR" \
      NIR_RID="$NIR_RID" NIR_DIR="$NIR_DIR" NIR_HOME="$NIR_HOME" \
      INT_RID="$INT_RID" INT_DIR="$INT_DIR" INT_HOME="$INT_HOME" \
      INT2_RID="$INT2_RID" INT2_DIR="$INT2_DIR" INT2_HOME="$INT2_HOME" \
      RUN_DIR_FAST="$RUN_DIR_FAST" RUN_DIR_STD="$RUN_DIR_STD" \
      RUN_DIR_DEEP="$RUN_DIR_DEEP" RUN_DIR_BATCH="$RUN_DIR_BATCH" \
      HOME_FAST="$HOME_FAST" HOME_STD="$HOME_STD" \
      HOME_DEEP="$HOME_DEEP" HOME_BATCH="$HOME_BATCH" \
      ISO_RA="$ISO_RA" ISO_RB="$ISO_RB" \
      ISO_DIR_A="$ISO_DIR_A" ISO_DIR_B="$ISO_DIR_B" \
      ISO_TMP_A="$ISO_TMP_A" ISO_TMP_B="$ISO_TMP_B" \
      CKPT_RID="$CKPT_RID" CKPT_DIR="$CKPT_DIR" CKPT_HOME="$CKPT_HOME" \
      bash -c "$body" >/tmp/e2e-ckpt-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/e2e-ckpt-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-e2e-ckpt.XXXXXX)"
  echo "$d"
}

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

# =====================================================================
# SECTION A — Non-interactive mode capture (10 tests)
# =====================================================================

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

run_test "s6_non_int_at_unix_is_positive" \
  "v=\$(sqlite3 $NIR_HOME/meta.sqlite \"SELECT at_unix FROM checkpoints WHERE run_id='$NIR_RID' LIMIT 1\"); test \"\$v\" -gt 1700000000"

run_test "s6_non_int_question_persisted" \
  "n=\$(sqlite3 $NIR_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE question IS NOT NULL AND run_id='$NIR_RID'\"); test \"\$n\" -ge 1"

run_test "s6_non_int_ckp_id_format_correct" \
  "sqlite3 $NIR_HOME/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE run_id='$NIR_RID' LIMIT 1\" | grep -qE '^h_'"

run_test "s6_non_int_question_mentions_intake" \
  "sqlite3 $NIR_HOME/meta.sqlite \"SELECT question FROM checkpoints WHERE kind='intake' AND run_id='$NIR_RID'\" | grep -qE 'intake surfaced|continue?'"

run_test "s6_non_int_jsonl_sidecar_also_exists" \
  "[[ -f $NIR_DIR/telemetry/checkpoints.jsonl ]]"

# =====================================================================
# SECTION B — Interactive mode capture (10 tests)
# =====================================================================

INT_TMP=$(mkhome)
INT_OUT=$(run_pipeline standard "Interactive question" "" "$INT_TMP" "y")
INT_RID="${INT_OUT%%|*}"
INT_DIR="${INT_OUT##*|}"
INT_HOME=$(dirname $(dirname "$INT_DIR"))

run_test "s6_int_user_typed_y_intake_response_is_y" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT response FROM checkpoints WHERE kind='intake' AND run_id='$INT_RID'\" | grep -qE '^y$'"

run_test "s6_int_user_typed_y_intake_accepted_default_false" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$INT_RID'\" | grep -qE '^0$'"

run_test "s6_int_deliver_checkpoint_written" \
  "test \$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\") -ge 1"

run_test "s6_int_deliver_response_empty_enter" \
  "v=\$(sqlite3 $INT_HOME/meta.sqlite \"SELECT response FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\"); test -z \"\$v\""

run_test "s6_int_deliver_accepted_default_true" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\" | grep -qE '^1$'"

run_test "s6_int_question_contains_ship_portfolio" \
  "sqlite3 $INT_HOME/meta.sqlite \"SELECT question FROM checkpoints WHERE kind='final' AND run_id='$INT_RID'\" | grep -qE 'ship portfolio'"

run_test "s6_int_total_two_distinct_kinds" \
  "test \$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(DISTINCT kind) FROM checkpoints WHERE run_id='$INT_RID'\") -eq 2"

run_test "s6_int_both_kinds_present_in_db" \
  "k=\$(sqlite3 $INT_HOME/meta.sqlite \"SELECT DISTINCT kind FROM checkpoints WHERE run_id='$INT_RID'\" | sort | xargs); test \"\$k\" = \"final intake\""

run_test "s6_int_jsonl_has_two_lines" \
  "test \$(wc -l < $INT_DIR/telemetry/checkpoints.jsonl) -eq 2"

run_test "s6_int_row_count_matches_jsonl_count" \
  "r=\$(sqlite3 $INT_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$INT_RID'\"); j=\$(wc -l < $INT_DIR/telemetry/checkpoints.jsonl); test \"\$r\" = \"\$j\""

# =====================================================================
# SECTION C — JSON sidecar <-> SQLite byte integrity (10 tests)
#
# Run an interactive mode e2e, then walk every (ckp_id, question,
# response, kind, accepted_default, at_unix) field of the SQLite row
# against the corresponding JSON sidecar. Catches bugs like
# telemetry writing one shape and the DB query reader expecting
# another.
# =====================================================================

INT2_TMP=$(mkhome)
INT2_OUT=$(run_pipeline standard "Integrity question" "" "$INT2_TMP" "yes")
INT2_RID="${INT2_OUT%%|*}"
INT2_DIR="${INT2_OUT##*|}"
INT2_HOME=$(dirname $(dirname "$INT2_DIR"))

run_test "s6_int_jsonl_ckp_id_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'\"); jl=\$(jq -r '.ckp_id' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); test \"\$db\" = \"\$jl\""

run_test "s6_int_jsonl_kind_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT kind FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'\"); jl=\$(jq -r '.kind' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); test \"\$db\" = \"\$jl\""

run_test "s6_int_jsonl_response_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT response FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'\"); jl=\$(jq -r '.response' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); test \"\$db\" = \"\$jl\""

run_test "s6_int_jsonl_question_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT question FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'\"); jl=\$(jq -r '.question' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); test \"\$db\" = \"\$jl\""

run_test "s6_int_jsonl_at_unix_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT at_unix FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'\"); jl=\$(jq -r '.at_unix' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); test \"\$db\" = \"\$jl\""

run_test "s6_int_jsonl_accepted_default_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$INT2_RID'\"); jl=\$(jq -r '.accepted_default' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); case \"\$jl\" in true) jl=1;; false) jl=0;; esac; test \"\$db\" = \"\$jl\""

run_test "s6_int_jsonl_run_id_matches_db" \
  "db=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT run_id FROM checkpoints WHERE run_id='$INT2_RID' LIMIT 1\"); jl=\$(jq -r '.run_id' \"$INT2_DIR/telemetry/checkpoints.jsonl\" | head -1); test \"\$db\" = \"\$jl\""

run_test "s6_int_sidecar_files_count_eq_db_rows" \
  "files=\$(find \"$INT2_DIR/checkpoints/\" -maxdepth 1 -type f -name '*.json' ! -name '*.meta.json' | wc -l); rows=\$(sqlite3 $INT2_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$INT2_RID' AND ckp_id LIKE 'h_%'\"); test \"\$files\" = \"\$rows\""

run_test "s6_int_sidecar_response_matches_db_for_every_row" \
  "sqlite3 $INT2_HOME/meta.sqlite \"SELECT ckp_id, response FROM checkpoints WHERE run_id='$INT2_RID'\" | while IFS='|' read ckp resp; do case \$ckp in legacy_*) continue;; esac; if [ -f \"$INT2_DIR/checkpoints/\${ckp}.json\" ]; then sidecar_resp=\$(jq -r '.response' \"$INT2_DIR/checkpoints/\${ckp}.json\"); if [ \"\$resp\" != \"\$sidecar_resp\" ]; then exit 1; fi; fi; done"

run_test "s6_int_sidecar_question_matches_db_for_every_row" \
  "sqlite3 $INT2_HOME/meta.sqlite \"SELECT ckp_id, question FROM checkpoints WHERE run_id='$INT2_RID'\" | while IFS='|' read ckp q; do case \$ckp in legacy_*) continue;; esac; if [ -f \"$INT2_DIR/checkpoints/\${ckp}.json\" ]; then sidecar_q=\$(jq -r '.question' \"$INT2_DIR/checkpoints/\${ckp}.json\"); if [ \"\$q\" != \"\$sidecar_q\" ]; then exit 1; fi; fi; done"

# =====================================================================
# SECTION D — Cross-mode coverage (24 tests, one row per (mode, kind))
# =====================================================================

run_pipeline_into() {
  local mode="$1"
  local prompt="$2"
  local home="$3"
  "$BIN" run --mode "$mode" --provider mock --prompt "$prompt" \
    --max-parallelism 2 --runs-dir "$home" --mock-dir "$MOCK_DIR" \
    --non-interactive > "$home/run.out" 2>&1 || true
  local rid
  rid="$(ls "$home/.runs/" 2>/dev/null | sort -r | head -1)"
  [[ -n "$rid" ]] && echo "$home/.runs/$rid"
}

TMP_FAST=$(mkhome)
TMP_STD=$(mkhome)
TMP_DEEP=$(mkhome)
TMP_BATCH=$(mkhome)

RUN_DIR_FAST=$(run_pipeline_into fast "Question for fast" "$TMP_FAST")
RUN_DIR_STD=$(run_pipeline_into standard "Question for standard" "$TMP_STD")
RUN_DIR_DEEP=$(run_pipeline_into deep "Question for deep" "$TMP_DEEP")
RUN_DIR_BATCH=$(run_pipeline_into batch "Question for batch" "$TMP_BATCH")

HOME_FAST=$(dirname $(dirname "$RUN_DIR_FAST"))
HOME_STD=$(dirname $(dirname "$RUN_DIR_STD"))
HOME_DEEP=$(dirname $(dirname "$RUN_DIR_DEEP"))
HOME_BATCH=$(dirname $(dirname "$RUN_DIR_BATCH"))

run_test "s6_mode_fast_user_version_5" \
  "sqlite3 $HOME_FAST/meta.sqlite 'PRAGMA user_version' | grep -qE '^5$'"
run_test "s6_mode_fast_checkpoints_table_present" \
  "sqlite3 $HOME_FAST/meta.sqlite '.tables' | grep -q 'checkpoints'"
run_test "s6_mode_fast_at_least_one_intake_row" \
  "test \$(sqlite3 $HOME_FAST/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake'\") -ge 1"
run_test "s6_mode_fast_jsonl_exists" \
  "[[ -s $RUN_DIR_FAST/telemetry/checkpoints.jsonl ]]"
run_test "s6_mode_fast_intake_skip_marker" \
  "test \$(sqlite3 $HOME_FAST/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE response='<skipped:non_interactive>' AND kind='intake'\") -ge 1"
run_test "s6_mode_fast_no_deliver_in_non_interactive" \
  "test \$(sqlite3 $HOME_FAST/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final'\") -eq 0"

run_test "s6_mode_standard_user_version_5" \
  "sqlite3 $HOME_STD/meta.sqlite 'PRAGMA user_version' | grep -qE '^5$'"
run_test "s6_mode_standard_checkpoints_table_present" \
  "sqlite3 $HOME_STD/meta.sqlite '.tables' | grep -q 'checkpoints'"
run_test "s6_mode_standard_at_least_one_intake_row" \
  "test \$(sqlite3 $HOME_STD/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake'\") -ge 1"
run_test "s6_mode_standard_jsonl_exists" \
  "[[ -s $RUN_DIR_STD/telemetry/checkpoints.jsonl ]]"
run_test "s6_mode_standard_intake_skip_marker" \
  "test \$(sqlite3 $HOME_STD/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE response='<skipped:non_interactive>' AND kind='intake'\") -ge 1"
run_test "s6_mode_standard_no_deliver_in_non_interactive" \
  "test \$(sqlite3 $HOME_STD/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final'\") -eq 0"

run_test "s6_mode_deep_user_version_5" \
  "sqlite3 $HOME_DEEP/meta.sqlite 'PRAGMA user_version' | grep -qE '^5$'"
run_test "s6_mode_deep_checkpoints_table_present" \
  "sqlite3 $HOME_DEEP/meta.sqlite '.tables' | grep -q 'checkpoints'"
run_test "s6_mode_deep_at_least_one_intake_row" \
  "test \$(sqlite3 $HOME_DEEP/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake'\") -ge 1"
run_test "s6_mode_deep_jsonl_exists" \
  "[[ -s $RUN_DIR_DEEP/telemetry/checkpoints.jsonl ]]"
run_test "s6_mode_deep_intake_skip_marker" \
  "test \$(sqlite3 $HOME_DEEP/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE response='<skipped:non_interactive>' AND kind='intake'\") -ge 1"
run_test "s6_mode_deep_no_deliver_in_non_interactive" \
  "test \$(sqlite3 $HOME_DEEP/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final'\") -eq 0"

run_test "s6_mode_batch_user_version_5" \
  "sqlite3 $HOME_BATCH/meta.sqlite 'PRAGMA user_version' | grep -qE '^5$'"
run_test "s6_mode_batch_checkpoints_table_present" \
  "sqlite3 $HOME_BATCH/meta.sqlite '.tables' | grep -q 'checkpoints'"
run_test "s6_mode_batch_at_least_one_intake_row" \
  "test \$(sqlite3 $HOME_BATCH/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake'\") -ge 1"
run_test "s6_mode_batch_jsonl_exists" \
  "[[ -s $RUN_DIR_BATCH/telemetry/checkpoints.jsonl ]]"
run_test "s6_mode_batch_intake_skip_marker" \
  "test \$(sqlite3 $HOME_BATCH/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE response='<skipped:non_interactive>' AND kind='intake'\") -ge 1"
run_test "s6_mode_batch_no_deliver_in_non_interactive" \
  "test \$(sqlite3 $HOME_BATCH/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final'\") -eq 0"

# =====================================================================
# SECTION E — Cross-run isolation (10 tests, parallel binary runs)
# =====================================================================

ISO_TMP_A=$(mkhome)
ISO_TMP_B=$(mkhome)

"$BIN" run --mode standard --provider mock --prompt "ISO A" --mock-dir "$MOCK_DIR" --runs-dir "$ISO_TMP_A" --non-interactive >/dev/null 2>&1 &
PID_A=$!
"$BIN" run --mode standard --provider mock --prompt "ISO B" --mock-dir "$MOCK_DIR" --runs-dir "$ISO_TMP_B" --non-interactive >/dev/null 2>&1 &
PID_B=$!
wait $PID_A $PID_B 2>/dev/null || true
ISO_RA=$(ls "$ISO_TMP_A/.runs/" 2>/dev/null | sort -r | head -1)
ISO_RB=$(ls "$ISO_TMP_B/.runs/" 2>/dev/null | sort -r | head -1)
ISO_DIR_A="$ISO_TMP_A/.runs/$ISO_RA"
ISO_DIR_B="$ISO_TMP_B/.runs/$ISO_RB"

run_test "s6_iso_run_a_has_one_intake" \
  "test \$(sqlite3 $ISO_TMP_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake' AND run_id='$ISO_RA'\") -ge 1"

run_test "s6_iso_run_b_has_one_intake" \
  "test \$(sqlite3 $ISO_TMP_B/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='intake' AND run_id='$ISO_RB'\") -ge 1"

run_test "s6_iso_run_a_no_b_checkpoints" \
  "test \$(sqlite3 $ISO_TMP_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RA'\") -ge 1 && test \$(sqlite3 $ISO_TMP_A/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RB'\") -eq 0"

run_test "s6_iso_run_b_no_a_checkpoints" \
  "test \$(sqlite3 $ISO_TMP_B/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RB'\") -ge 1 && test \$(sqlite3 $ISO_TMP_B/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE run_id='$ISO_RA'\") -eq 0"

run_test "s6_iso_each_run_has_distinct_ckp_id_prefix" \
  "a=\$(sqlite3 $ISO_TMP_A/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE run_id='$ISO_RA'\" | head -1); b=\$(sqlite3 $ISO_TMP_B/meta.sqlite \"SELECT ckp_id FROM checkpoints WHERE run_id='$ISO_RB'\" | head -1); case \$a in h_*) ;; *) echo 'a not h_' ; exit 1;; esac; case \$b in h_*) ;; *) echo 'b not h_' ; exit 1;; esac"

run_test "s6_iso_run_a_question_distinct_from_b_due_to_different_prompts" \
  "[[ -n \"\$ISO_RA\" ]] && [[ -n \"\$ISO_RB\" ]]"

run_test "s6_iso_run_a_meta_user_version_5" \
  "test \$(sqlite3 $ISO_TMP_A/meta.sqlite 'PRAGMA user_version') -eq 5"

run_test "s6_iso_run_b_meta_user_version_5" \
  "test \$(sqlite3 $ISO_TMP_B/meta.sqlite 'PRAGMA user_version') -eq 5"

run_test "s6_iso_runs_independently_indexed_in_runs_table" \
  "test \$(sqlite3 $ISO_TMP_A/meta.sqlite \"SELECT COUNT(*) FROM runs WHERE run_id IN ('$ISO_RA','$ISO_RB')\") -eq 1 && test \$(sqlite3 $ISO_TMP_B/meta.sqlite \"SELECT COUNT(*) FROM runs WHERE run_id IN ('$ISO_RA','$ISO_RB')\") -eq 1"

run_test "s6_iso_jsonl_sidecars_in_separate_run_dirs" \
  "[[ -s $ISO_DIR_A/telemetry/checkpoints.jsonl ]] && [[ -s $ISO_DIR_B/telemetry/checkpoints.jsonl ]]"

# =====================================================================
# SECTION F — Interactive end-to-end (5 tests from smoke_human_checkpoint.sh §H8)
# =====================================================================

CKPT_TMP=$(mkhome)
CKPT_OUT=$(run_pipeline standard "Interactive ckpt" "" "$CKPT_TMP" "y")
CKPT_RID="${CKPT_OUT%%|*}"
CKPT_DIR="${CKPT_OUT##*|}"
CKPT_HOME=$(dirname $(dirname "$CKPT_DIR"))

run_test "ckpt_int_e2e_intake_response_is_y" \
  "sqlite3 $CKPT_HOME/meta.sqlite \"SELECT response FROM checkpoints WHERE kind='intake' AND run_id='$CKPT_RID'\" | grep -qE '^y$'"

run_test "ckpt_int_e2e_intake_accepted_default_false" \
  "sqlite3 $CKPT_HOME/meta.sqlite \"SELECT accepted_default FROM checkpoints WHERE kind='intake' AND run_id='$CKPT_RID'\" | grep -qE '^0$'"

run_test "ckpt_int_e2e_deliver_written" \
  "test \$(sqlite3 $CKPT_HOME/meta.sqlite \"SELECT COUNT(*) FROM checkpoints WHERE kind='final' AND run_id='$CKPT_RID'\") -ge 1"

run_test "ckpt_int_e2e_two_distinct_kinds" \
  "test \$(sqlite3 $CKPT_HOME/meta.sqlite \"SELECT COUNT(DISTINCT kind) FROM checkpoints WHERE run_id='$CKPT_RID'\") -eq 2"

run_test "ckpt_int_e2e_two_json_files" \
  "find $CKPT_DIR/checkpoints/ -maxdepth 1 -type f -name 'h_*.json' ! -name '*.meta.json' | wc -l | grep -qE '^2$'"

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Interactive checkpoint E2E tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
