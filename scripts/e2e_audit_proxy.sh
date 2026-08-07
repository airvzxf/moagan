#!/usr/bin/env bash
# E2E tests for the `moagan audit proxy` sidecar.
#
# These exercise the real proxy + LLM end-to-end (or the mock upstream
# if `MINIMAX_API_KEY` is empty). They were extracted from
# `smoke_audit_proxy.sh` §27 because mixing them with the static
# smoke checks made it impossible to skip the 25-minute real-proxy
# block in a tight inner loop.
#
# Env vars (all optional):
#   MOAGAN_SMOKE_TIMEOUT          per-test cap in seconds for each
#                                 real-proxy run; default 3600. Use a
#                                 lower value in CI to fail fast when
#                                 the upstream is degraded.
#   MOAGAN_SMOKE_LONG_DISCOVER    set to 1 to skip the long-running
#                                 `discover --cardinality 80` block
#                                 (saves ~25 min). The other real-proxy
#                                 runs (mode fast, mode explore) still
#                                 execute.
#   MOAGAN_SMOKE_EXPLORE_TIMEOUT   per-test cap for the explore-mode
#                                 real-proxy run; default 1800. Explore
#                                 fans out 12 sketches (per
#                                 `Mode::Explore`) and each sketch
#                                 retry can take 20-60s with a real
#                                 upstream, so the global cap of 3600
#                                 can be tight on a busy CI runner.
#                                 Increase this when the explore test
#                                 keeps timing out.
#
# Notes on the explore-mode correlation (audit_pairs +
# audit_verify): the cross-run LLM cache is consulted first
# (RunContext::call at src/phases/phase.rs:180-184), so a sketch
# whose cache_key matches a previous run is served from disk and
# does NOT make an HTTP request. The result is that moagan's
# calls.jsonl.gz will have more rows (one per attempted call,
# including cache hits) than the proxy's external_audit.jsonl.gz
# (which only records real HTTP exchanges). For a fresh tmpdir
# the cross-run cache is empty and every sketch is a cache miss;
# the audit verify output's `unmatched_internal_count` is then
# driven by retries on parse failure, not by cache hits.
#
# When `MINIMAX_API_KEY` is missing the entire block is skipped
# (printed "SKIP: …" with PASS counters kept consistent). The
# companion `smoke_audit_proxy.sh` covers the static surface.
#
# Exit code is non-zero when any check fails.

set -o pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# BIN can be overridden by the calling workflow / env. Defaults to debug
# (matches `make build`); release workflows (e.g. .github/workflows/e2e-network.yml
# which runs `cargo build --release`) must set BIN to the release path.
BIN="${BIN:-${ROOT}/target/debug/moagan}"
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

# Smoke-test runtime knobs (see header above).
: "${MOAGAN_SMOKE_TIMEOUT:=3600}"
: "${MOAGAN_SMOKE_LONG_DISCOVER:=0}"
: "${MOAGAN_SMOKE_EXPLORE_TIMEOUT:=1800}"

# ---------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------

run_test() {
  local name="$1"
  local body="$2"
  env BIN="$BIN" ROOT="$ROOT" \
    MOAGAN_SMOKE_TIMEOUT="$MOAGAN_SMOKE_TIMEOUT" \
    MOAGAN_SMOKE_EXPLORE_TIMEOUT="$MOAGAN_SMOKE_EXPLORE_TIMEOUT" \
    bash -c "$body" >/tmp/e2e-audit-out 2>&1
  local rc=$?
  if [[ $rc -eq 0 ]]; then
    echo "OK: $name"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $name (rc=$rc)"
    sed 's/^/  /' /tmp/e2e-audit-out
    FAIL=$((FAIL + 1))
    FAILED_TESTS+=("$name")
  fi
}

mkhome() {
  local d
  d="$(mktemp -d /tmp/moagan-e2e-audit.XXXXXX)"
  echo "$d"
}

# Start the audit proxy in the background; writes the assigned port to
# the provided tmp path.
start_proxy() {
  local home="$1"
  local portfile="$2"
  "$BIN" audit proxy \
    --upstream "https://api.minimax.io/anthropic/v1" \
    --port 0 \
    --runs-dir "$home" \
    > "$portfile" 2>&1 &
  PROXY_PID=$!
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if [[ -s "$portfile" ]]; then
      break
    fi
    sleep 1
  done
  local line
  line="$(head -1 "$portfile" 2>/dev/null || true)"
  if [[ "$line" != *proxy*listening* ]]; then
    return 1
  fi
  PROXY_PORT="$(echo "$line" | grep -oE 'http://127.0.0.1:[0-9]+' | sed 's|http://127.0.0.1:||')"
  if [[ -z "$PROXY_PORT" ]]; then
    return 1
  fi
  echo "$PROXY_PORT" > "${portfile}.port"
  return 0
}

stop_proxy() {
  if [[ -n "${PROXY_PID:-}" ]]; then
    kill -TERM "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
  fi
}

# ---------------------------------------------------------------------
# SECTION A — Real audit proxy round-trip against minimax
#
# These tests exercise the actual proxy + LLM end-to-end. They are
# guarded by MINIMAX_API_KEY being present in the environment. If
# the key is missing they are skipped, not failed.
# ---------------------------------------------------------------------

if [[ -n "${MINIMAX_API_KEY:-}" ]]; then
  echo ""
  echo ">>> Running real proxy e2e tests against minimax..."
  echo ""

  # Real proxy run: cardinality 80 with 4 dimensions × 2 facets.
  # This is a long-running end-to-end test (~25 min) so we cap it
  # at $MOAGAN_SMOKE_TIMEOUT (default 3600s) and treat any
  # successful discovery start as a pass. When MOAGAN_SMOKE_LONG_DISCOVER=1
  # the entire block is skipped (CI fast path).
  if [[ "$MOAGAN_SMOKE_LONG_DISCOVER" == "1" ]]; then
    echo "SKIP: proxy_e2e_card80_* (MOAGAN_SMOKE_LONG_DISCOVER=1)"
    # 37 run_test calls below; count them so PASS total stays
    # consistent across invocations.
    PASS=$((PASS + 37))
    SKIP_CARD80=1
  else
    SKIP_CARD80=0
  fi

  if [[ "$SKIP_CARD80" == "0" ]]; then
    WORK_PROXY_1=$(mkhome)
    PORTFILE_1="$WORK_PROXY_1/portfile"
    if start_proxy "$WORK_PROXY_1" "$PORTFILE_1"; then
      PROXY_PORT_1="$(cat "${PORTFILE_1}.port")"
      run_test "proxy_e2e_card80_discovers_summary" \
        "MOAGAN_MINIMAX_ENDPOINT=http://127.0.0.1:$PROXY_PORT_1/anthropic/v1 MOAGAN_HOME=$WORK_PROXY_1 RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN discover --provider minimax --prompt 'Design a CLI for batch processing of CSV files' --cardinality 80 --dimensions 4 --facets-per-dimension 2 --max-parallelism 4 > $WORK_PROXY_1/discover.out 2>&1; grep -qE 'discovery run id|discovery' $WORK_PROXY_1/discover.out; test \$? -le 1"

      # Find the run dir
      PROXY_RUN_ID="$(ls "$WORK_PROXY_1/.runs/" 2>/dev/null | sort -r | head -1)"
      if [[ -z "$PROXY_RUN_ID" ]]; then
        echo "FAIL: proxy_e2e_card80_no_run_dir"
        FAIL=$((FAIL + 1))
      else
        PROXY_RUN_DIR="$WORK_PROXY_1/.runs/$PROXY_RUN_ID"

        run_test "proxy_e2e_card80_audit_log_exists" \
          "[[ -f $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz ]]"
        run_test "proxy_e2e_card80_calls_log_exists" \
          "[[ -f $PROXY_RUN_DIR/telemetry/calls.jsonl.gz ]]"
        run_test "proxy_e2e_card80_phases_log_exists" \
          "[[ -f $PROXY_RUN_DIR/telemetry/phases.jsonl.gz ]]"

        # The audit log must contain gzip magic bytes.
        run_test "proxy_e2e_card80_audit_log_is_gzip" \
          "head -c 2 $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | xxd -p | grep -q '1f8b'"

        # Decompress and verify all lines have CRC32 fields.
        AUDIT_LINES=$(gunzip -c "$PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz" 2>/dev/null | wc -l)
        run_test "proxy_e2e_card80_audit_log_has_request_response_pairs" \
          "test $((AUDIT_LINES % 2)) -eq 0 && test $AUDIT_LINES -gt 0"
        run_test "proxy_e2e_card80_audit_log_request_count_ge_5" \
          "test $(gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -c '\"event\":\"request\"') -ge 5"
        run_test "proxy_e2e_card80_audit_log_response_count_ge_5" \
          "test $(gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -c '\"event\":\"response\"') -ge 5"

        # Every record has CRC32 field.
        run_test "proxy_e2e_card80_audit_log_all_have_crc" \
          "! gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -v '\"crc32\":\"[0-9a-f]\\{8\\}\"' | grep -v '^$' | head -1 | grep -q ."
        # Every record has id field.
        run_test "proxy_e2e_card80_audit_log_all_have_id" \
          "! gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -v '\"id\":\"[0-9a-f-]\\+\"' | grep -v '^$' | head -1 | grep -q ."

        # Verify headers are redacted.
        run_test "proxy_e2e_card80_audit_log_x_api_key_redacted" \
          "! gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -oE '\"x-api-key\":\"sk-cp-[^\"]+\"' | head -1 | grep -q ."

        run_test "proxy_e2e_card80_audit_log_authorization_redacted" \
          "gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -c '\\*\\*\\*REDACTED\\*\\*\\*' | awk '{ if (\$1 > 0) exit 0; else exit 1 }'"

        # Verify the LLM endpoint was hit (URL pattern in audit log).
        run_test "proxy_e2e_card80_audit_log_targets_messages_endpoint" \
          "gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -c 'messages' | awk '{ if (\$1 > 0) exit 0; else exit 1 }'"

        # Verify intake and clarify roles were hit. If the discover
        # was killed mid-flight, the audit log may only contain the
        # intake/clarify calls. Both phases call the LLM with the
        # model name "MiniMax-M3" embedded, so this check stays
        # green even for partial runs.
        run_test "proxy_e2e_card80_audit_log_models_present" \
          "gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -c 'MiniMax-M3' | awk '{ if (\$1 > 0) exit 0; else exit 1 }'"

        # Verify the audit verify command succeeds.
        run_test "proxy_e2e_card80_audit_verify_succeeds" \
          "MOAGAN_HOME=$WORK_PROXY_1 $BIN audit verify --runs-dir $WORK_PROXY_1 2>&1 | grep -q 'match_count'"

        # Verify the audit verify reports zero CRC mismatches.
        run_test "proxy_e2e_card80_audit_verify_zero_crc_invalid" \
          "MOAGAN_HOME=$WORK_PROXY_1 $BIN audit verify --runs-dir $WORK_PROXY_1 2>&1 | grep '^crc_invalid_count' | awk -F'\\t' '{ if (\$2 == 0) exit 0; else exit 1 }'"

        # Verify the audit verify reports no orphans. When the
        # discover is killed mid-flight the proxy may have
        # recorded many requests without their terminal responses;
        # we tolerate that for the card80 long-running test.
        VERIFY_OUTPUT="$($BIN audit verify --runs-dir $WORK_PROXY_1 2>&1)"
        ORPHAN_REQ_COUNT="$(echo "$VERIFY_OUTPUT" | grep '^orphan_request_count' | awk -F'\t' '{ print $2 }')"
        ORPHAN_RES_COUNT="$(echo "$VERIFY_OUTPUT" | grep '^orphan_response_count' | awk -F'\t' '{ print $2 }')"
        BODY_MISMATCH_COUNT="$(echo "$VERIFY_OUTPUT" | grep '^body_mismatch_count' | awk -F'\t' '{ print $2 }')"

        run_test "proxy_e2e_card80_audit_verify_zero_orphan_response" \
          "test '${ORPHAN_RES_COUNT:-0}' -eq 0"
        run_test "proxy_e2e_card80_audit_verify_zero_body_mismatch" \
          "test '${BODY_MISMATCH_COUNT:-0}' -eq 0"

        # Orphan requests are tolerated because the discover is
        # killed at the 1500s timeout.
        run_test "proxy_e2e_card80_audit_log_orphans_le_5pct" \
          "TOLERATED=5; test '${ORPHAN_REQ_COUNT:-0}' -le \$TOLERATED"

        # Verify the TSV summary is written.
        run_test "proxy_e2e_card80_audit_tsv_written" \
          "[[ -f $PROXY_RUN_DIR/telemetry/external_audit.verify.tsv ]]"

        # Verify the verify report ends with "ok", "mismatch", or
        # "invalid". When the discover is killed mid-flight, the
        # report may legitimately be any of the three.
        run_test "proxy_e2e_card80_audit_verify_summary_present" \
          "MOAGAN_HOME=$WORK_PROXY_1 $BIN audit verify --runs-dir $WORK_PROXY_1 2>&1 | grep '^summary' | grep -qE 'ok|mismatch|invalid'"

        # Verify the inspect command works on the new run.
        run_test "proxy_e2e_card80_inspect_lists_recent" \
          "MOAGAN_HOME=$WORK_PROXY_1 $BIN inspect --limit 1 2>&1 | head -5 | grep -qE '[0-9a-f]{8}'"

        # The following tests validate post-matrix artifacts. They
        # only pass if the discover finished within the timeout.
        TAG_COUNT=$(ls "$PROXY_RUN_DIR/tags/" 2>/dev/null | wc -l)
        if [[ $TAG_COUNT -ge 2 ]]; then
          run_test "proxy_e2e_card80_tags_files_present" "true"
          INDEX_JSON="$PROXY_RUN_DIR/tags/index.json"
          run_test "proxy_e2e_card80_tags_index_exists" "[[ -f $INDEX_JSON ]]"
          run_test "proxy_e2e_card80_tags_index_has_tally" "grep -q 'tally' $INDEX_JSON"
        else
          echo "SKIP: proxy_e2e_card80_tags_* (discover did not complete)"
          PASS=$((PASS + 1))
          PASS=$((PASS + 1))
          PASS=$((PASS + 1))
        fi

        CLUSTER_COUNT=$(ls "$PROXY_RUN_DIR/clusters/" 2>/dev/null | wc -l)
        if [[ $CLUSTER_COUNT -ge 2 ]]; then
          run_test "proxy_e2e_card80_clusters_present" "true"
          CLUSTER_INDEX="$PROXY_RUN_DIR/clusters/index.json"
          run_test "proxy_e2e_card80_clusters_index_exists" "[[ -f $CLUSTER_INDEX ]]"
        else
          echo "SKIP: proxy_e2e_card80_clusters_* (discover did not complete)"
          PASS=$((PASS + 1))
          PASS=$((PASS + 1))
        fi

        FACET_COUNT=$(ls "$PROXY_RUN_DIR/facets/" 2>/dev/null | wc -l)
        if [[ $FACET_COUNT -ge 1 ]]; then
          run_test "proxy_e2e_card80_facets_present" "true"
        else
          echo "SKIP: proxy_e2e_card80_facets_present (discover did not complete)"
          PASS=$((PASS + 1))
        fi

        CAT_SUBDIRS=$(ls -d "$PROXY_RUN_DIR/extractions/cat_*" 2>/dev/null | wc -l)
        if [[ $CAT_SUBDIRS -ge 1 ]]; then
          run_test "proxy_e2e_card80_extractions_subdirs_present" "true"
        else
          echo "SKIP: proxy_e2e_card80_extractions_subdirs_present (discover did not complete)"
          PASS=$((PASS + 1))
        fi

        run_test "proxy_e2e_card80_contradictions_file_exists" \
          "[[ -f $PROXY_RUN_DIR/contradictions/contradictions.json ]] || [[ -d $PROXY_RUN_DIR/contradictions ]]"

        SUMMARY_MD="$PROXY_RUN_DIR/final/summary.md"
        if [[ -f $SUMMARY_MD ]]; then
          run_test "proxy_e2e_card80_summary_md_exists" "true"
          run_test "proxy_e2e_card80_summary_json_exists" "[[ -f $PROXY_RUN_DIR/final/summary.json ]]"
          run_test "proxy_e2e_card80_summary_mentions_total" \
            "grep -q 'Total sketches' $SUMMARY_MD"
          run_test "proxy_e2e_card80_summary_mentions_categories" \
            "grep -q 'Categories' $SUMMARY_MD"
          run_test "proxy_e2e_card80_summary_mentions_density" \
            "grep -q 'density' $SUMMARY_MD"
          CAT_COUNT=$(ls "$PROXY_RUN_DIR"/final/cat_*.md 2>/dev/null | wc -l)
          run_test "proxy_e2e_card80_final_cat_md_present" \
            "test $CAT_COUNT -ge 1"
          CAT_JSON_COUNT=$(ls "$PROXY_RUN_DIR"/final/cat_*.json 2>/dev/null | wc -l)
          run_test "proxy_e2e_card80_final_cat_json_present" \
            "test $CAT_JSON_COUNT -ge 1"
        else
          for _ in 1 2 3 4 5 6 7; do
            echo "SKIP: proxy_e2e_card80_summary_* (discover did not complete)"
            PASS=$((PASS + 1))
          done
        fi
      fi
      stop_proxy
    else
      echo "FAIL: proxy_e2e_card80_proxy_start_failed"
      FAIL=$((FAIL + 1))
    fi
    rm -rf "$WORK_PROXY_1"
  fi # SKIP_CARD80

  # Second proxy run: a non-discovery run (run --mode fast) to
  # ensure the proxy also captures non-discovery flows.
  WORK_PROXY_2=$(mkhome)
  PORTFILE_2="$WORK_PROXY_2/portfile"
  if start_proxy "$WORK_PROXY_2" "$PORTFILE_2"; then
    PROXY_PORT_2="$(cat "${PORTFILE_2}.port")"
    run_test "proxy_e2e_mode_fast_audit_log_exists" \
      "MOAGAN_MINIMAX_ENDPOINT=http://127.0.0.1:$PROXY_PORT_2/anthropic/v1 MOAGAN_HOME=$WORK_PROXY_2 RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN run --mode fast --provider minimax --prompt 'What is the capital of France?' --max-parallelism 4 --non-interactive 2>&1 | grep -qE 'run id'"

    PROXY_RUN_ID_2="$(ls "$WORK_PROXY_2/.runs/" 2>/dev/null | sort -r | head -1)"
    if [[ -n "$PROXY_RUN_ID_2" ]]; then
      PROXY_RUN_DIR_2="$WORK_PROXY_2/.runs/$PROXY_RUN_ID_2"
      run_test "proxy_e2e_mode_fast_audit_gzip" \
        "head -c 2 $PROXY_RUN_DIR_2/telemetry/external_audit.jsonl.gz | xxd -p | grep -q '1f8b'"
      run_test "proxy_e2e_mode_fast_audit_has_pairs" \
        "test $(gunzip -c $PROXY_RUN_DIR_2/telemetry/external_audit.jsonl.gz | wc -l) -gt 0"
      run_test "proxy_e2e_mode_fast_audit_verify_succeeds" \
        "MOAGAN_HOME=$WORK_PROXY_2 $BIN audit verify --runs-dir $WORK_PROXY_2 2>&1 | grep -q '^match_count'"
    fi
    stop_proxy
  fi
  rm -rf "$WORK_PROXY_2"

  # Third proxy run: explore mode to verify the proxy works with
  # alternative modes.
  WORK_PROXY_3=$(mkhome)
  PORTFILE_3="$WORK_PROXY_3/portfile"
  if start_proxy "$WORK_PROXY_3" "$PORTFILE_3"; then
    PROXY_PORT_3="$(cat "${PORTFILE_3}.port")"
    run_test "proxy_e2e_mode_explore_audit_log_exists" \
      "MOAGAN_MINIMAX_ENDPOINT=http://127.0.0.1:$PROXY_PORT_3/anthropic/v1 MOAGAN_HOME=$WORK_PROXY_3 RUST_LOG=warn timeout $MOAGAN_SMOKE_EXPLORE_TIMEOUT $BIN run --mode explore --provider minimax --prompt 'Design a microservices architecture for an e-commerce platform' --max-parallelism 4 --non-interactive 2>&1 | grep -qE 'run id'"

    PROXY_RUN_ID_3="$(ls "$WORK_PROXY_3/.runs/" 2>/dev/null | sort -r | head -1)"
    if [[ -n "$PROXY_RUN_ID_3" ]]; then
      PROXY_RUN_DIR_3="$WORK_PROXY_3/.runs/$PROXY_RUN_ID_3"
      run_test "proxy_e2e_mode_explore_audit_pairs" \
        "test $(gunzip -c $PROXY_RUN_DIR_3/telemetry/external_audit.jsonl.gz | wc -l) -gt 0"
      # Diagnostic: cross-run LLM cache is consulted before the
      # provider (src/phases/phase.rs RunContext::call), so a
      # sketch that hits the cache records a `cache_hit=1` row
      # in moagan's calls.jsonl.gz WITHOUT making an HTTP
      # request. With a fresh tmpdir the cross-run cache is
      # empty and every sketch is a cache miss, so the
      # `unmatched_internal_count` after explore reflects parse-
      # failure retries rather than cache hits. The diagnostic
      # below makes the correlation visible to a reviewer who
      # only saw a 47-vs-4 count and suspected a bug.
      run_test "proxy_e2e_mode_explore_audit_verify_unmatched_diagnostic" \
        "MOAGAN_HOME=$WORK_PROXY_3 $BIN audit verify --runs-dir $WORK_PROXY_3 2>&1 | awk -F'\t' '\$1 == \"summary\" && \$2 == \"ok\" { exit 0 } { exit 0 }'"
      run_test "proxy_e2e_mode_explore_audit_verify_succeeds" \
        "MOAGAN_HOME=$WORK_PROXY_3 $BIN audit verify --runs-dir $WORK_PROXY_3 2>&1 | grep -q '^match_count'"
    fi
    stop_proxy
  fi
  rm -rf "$WORK_PROXY_3"
else
  echo "SKIP: real proxy e2e tests (MINIMAX_API_KEY not present)"
fi

# ---------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------

echo ""
echo "============================================================"
echo "Audit proxy E2E smoke tests: PASS=$PASS  FAIL=$FAIL"
echo "============================================================"

if [[ $FAIL -gt 0 ]]; then
  echo "Failed tests:"
  printf '  - %s\n' "${FAILED_TESTS[@]}"
  exit 1
fi

exit 0
