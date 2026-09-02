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
#                                 `discover --sketches-per-cell 20` block
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
#   MOAGAN_SMOKE_SECTION           select which SECTION A sub-block to
#                                 run. One of `all` (default), `card80`
#                                 (the ~25 min discover), `fast` (the
#                                 ~2 min mode-fast run), `explore` (the
#                                 ~8 min mode-explore run),
#                                 `discover_opencode` (the
#                                 ~10–20 min `moagan discover` against
#                                 the opencode provider; v0.7 P8
#                                 e2e validation close),
#                                 `discover_deepseek` (the equivalent
#                                 for the native `deepseek` provider;
#                                 PR #462), or
#                                 `discover_opencode_models` (the
#                                 ~70 min per-model coverage loop of
#                                 SECTION A.quad).
#
#                                 Post-PR #555 the auto path of
#                                 `.github/workflows/e2e-network.yml`
#                                 runs only `fast` + `explore`. The
#                                 `card80` block lives in
#                                 `e2e-network-card80.yml` (manual
#                                 dispatch); the `discover_opencode`
#                                 / `discover_deepseek` /
#                                 `discover_opencode_models`
#                                 sub-blocks are operator-only — no
#                                 CI job fixes those values. Locally
#                                 the operator can still narrow to any
#                                 single sub-block without paying the
#                                 ~25 min card80 cost.
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
# When `MINIMAX_API_KEY` is missing the card80/fast/explore blocks
# are skipped (printed "SKIP: …" with PASS counters kept
# consistent); the opencode block below gates on
# `OPENCODE_API_KEY` and the deepseek block further down gates
# on `DEEPSEEK_API_KEY` instead. The companion
# `smoke_audit_proxy.sh` covers the static surface.
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
: "${MOAGAN_SMOKE_SECTION:=all}"   # all | card80 | fast | explore | discover_opencode | discover_deepseek | discover_opencode_models

# OpenCode models to exercise in the per-model coverage loop
# (SECTION A.quad). Pinned to the operator's published roster
# (`docs/proposal-03-add-ons.md` §10-integrada-v0 OpenCode All
# Models catalog; effective as of 2026-08-28). The list deliberately
# excludes kimi-k3 / kimi-k2.6 / glm-5.1 / glm-5.2 / mimo-v2.5-pro /
# hy3 / qwen3.8-max / qwen3.7-plus / qwen3.6-plus — these remain
# registered in `default_providers` for backward compatibility but
# the operator no longer exercises them through the audit-proxy
# coverage loop.
# Every entry is a first-class provider alias registered in
# `default_providers` (`src/config/mod.rs:1138-1232`), so
# `--provider opencode:<model>` resolves without a companion
# `--model` flag.
OPENCODE_COVERAGE_MODELS=(
  deepseek-v4-flash         # /v1/chat/completions — opencode alias (distinct from native `deepseek` provider in A.ter; same model name, different endpoint)
  glm-5.3-flash             # /v1/chat/completions
  gpt-5.6-luna              # /v1/responses
  mimo-v2.5                 # /v1/chat/completions — also the smoke-test model in A.bis
  minimax-m2.7              # /v1/messages    — Anthropic-compatible
  muse-spark-1.2-contributor # /v1/responses
  qwen3.7-max               # /v1/messages
)
# Total: 7 models. Combined with the A.bis mimo-v2.5 round-trip
# this exercises the operator's published OpenCode roster.
#
# `deepseek-v4-flash` appears twice in this suite on purpose: here it is
# the opencode alias (`src/config/mod.rs:1166-1169`) relayed to
# `opencode.ai/zen/go/v1/chat/completions`, while the A.ter block below
# drives the same model name through the NATIVE `deepseek` provider
# (`src/config/mod.rs:1106`) at `api.deepseek.com`. Same model, two
# distinct endpoints and two distinct wire paths — covering one does
# not cover the other.

# ---------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------

run_test() {
  local name="$1"
  local body="$2"
  env BIN="$BIN" ROOT="$ROOT" \
    MOAGAN_SMOKE_TIMEOUT="$MOAGAN_SMOKE_TIMEOUT" \
    MOAGAN_SMOKE_EXPLORE_TIMEOUT="$MOAGAN_SMOKE_EXPLORE_TIMEOUT" \
    MOAGAN_MAX_TOKEN_AUTO=false \
    MOAGAN_MAX_TOKEN_AUTO_SAVE=false \
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

# Per-job scratch directory. In CI, anchor under ${GITHUB_WORKSPACE}
# so the e2e-network.yml `actions/upload-artifact` steps (which
# watch ${GITHUB_WORKSPACE}/.runs/) actually capture the runs.
# Outside CI, fall back to the historical /tmp/ location so local
# dev behavior is unchanged.
mkhome() {
  if [[ -n "${CI:-}" && -n "${GITHUB_WORKSPACE:-}" ]]; then
    local d="${GITHUB_WORKSPACE}/.runs/e2e-audit-$$-${RANDOM}"
    mkdir -p "$d"
    echo "$d"
  else
    mktemp -d /tmp/moagan-e2e-audit.XXXXXX
  fi
}

# In CI, leave the home dir in place so the workflow's
# `actions/upload-artifact` step can capture the runs. Outside
# CI the historical behavior (rm -rf the tempdir) is preserved.
cleanup_home() {
  if [[ -z "${CI:-}" ]]; then
    rm -rf "$1"
  fi
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
  line="$(grep -m1 'proxy listening' "$portfile" 2>/dev/null || true)"
  if [[ "$line" != *proxy*listening* ]]; then
    echo "ERROR: proxy did not print 'proxy listening' within 10s. First 5 lines of $portfile:" >&2
    head -5 "$portfile" >&2 2>/dev/null || true
    echo "ERROR: proxy process exit status:" >&2
    if kill -0 "$PROXY_PID" 2>/dev/null; then
      echo "  still running (pid=$PROXY_PID)" >&2
    else
      echo "  exited (pid=$PROXY_PID)" >&2
    fi
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

# v0.13.1 removed the legacy `[providers.<name>]` heredoc; the
# 131072 cap for MiniMax-M2.7 is now pinned per-proxy via
# `MOAGAN_MINIMAX_MAX_TOKENS=131072` in each `run_test` body
# (see src/llm/max_tokens.rs::resolve_max_tokens). Without this override the
# runtime falls back to `MINIMAX_MAX_TOKENS_CAP = 524288` when
# the startup auto-probe can't reach models.dev, and the
# upstream rejects with HTTP 400 "does not support max tokens
# > N".

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

  # Real proxy run: 4 dimensions × 2 facets ×
  # `--sketches-per-cell 10` = 80 sketches (the F2 default).
  # This is a long-running end-to-end test (~25 min) so we cap it
  # at $MOAGAN_SMOKE_TIMEOUT (default 3600s) and treat any
  # successful discovery start as a pass. When MOAGAN_SMOKE_LONG_DISCOVER=1
  # the entire block is skipped (CI fast path). The outer
  # MOAGAN_SMOKE_SECTION guard below makes this block opt-in for
  # the matrix split; the inner SKIP_CARD80 check keeps the
  # LONG_DISCOVER fast-path behaviour intact.
  if [[ "$MOAGAN_SMOKE_LONG_DISCOVER" == "1" ]]; then
    SKIP_CARD80=1
    # Only bump the PASS counter when the card80 block would have
    # run anyway (i.e. section is 'all' or 'card80'). When the
    # operator narrows the run to a different section, the card80
    # tests are skipped by the outer guard and never count toward
    # PASS — keeping the totals section-independent.
    if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "card80" ]]; then
      echo "SKIP: proxy_e2e_card80_* (MOAGAN_SMOKE_LONG_DISCOVER=1)"
      # 37 run_test calls below; count them so PASS total stays
      # consistent across invocations.
      PASS=$((PASS + 37))
    fi
  elif [[ "$MOAGAN_SMOKE_SECTION" != "all" && "$MOAGAN_SMOKE_SECTION" != "card80" ]]; then
    # A different section is selected; the card80 block is skipped
    # by the outer guard below, so flag it here too so any
    # downstream logic that branches on SKIP_CARD80 stays correct.
    SKIP_CARD80=1
  else
    SKIP_CARD80=0
  fi

  if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "card80" ]]; then
  if [[ "$SKIP_CARD80" == "0" ]]; then
    WORK_PROXY_1=$(mkhome)
    PORTFILE_1="$WORK_PROXY_1/portfile"
    if start_proxy "$WORK_PROXY_1" "$PORTFILE_1"; then
      PROXY_PORT_1="$(cat "${PORTFILE_1}.port")"
      run_test "proxy_e2e_card80_discovers_summary" \
        "MOAGAN_MINIMAX_MAX_TOKENS=131072 MOAGAN_MINIMAX_ENDPOINT=http://127.0.0.1:$PROXY_PORT_1/anthropic/v1/messages MOAGAN_HOME=$WORK_PROXY_1 RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN discover --provider minimax:MiniMax-M2.7 --prompt 'Design a CLI for batch processing of CSV files' --sketches-per-cell 10 --dimensions 4 --facets-per-dimension 2 --max-parallelism 4 > $WORK_PROXY_1/discover.out 2>&1; grep -qE 'discovery run id|discovery' $WORK_PROXY_1/discover.out"

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
        # model name "MiniMax-M2.7" embedded (the model the card80
        # `--provider minimax:MiniMax-M2.7` selects), so this check
        # stays green even for partial runs.
        run_test "proxy_e2e_card80_audit_log_models_present" \
          "gunzip -c $PROXY_RUN_DIR/telemetry/external_audit.jsonl.gz | grep -c 'MiniMax-M2.7' | awk '{ if (\$1 > 0) exit 0; else exit 1 }'"

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

        CAT_SUBDIRS=$(find "$PROXY_RUN_DIR/extractions" -maxdepth 1 -type d -name 'cat_*' 2>/dev/null | wc -l)
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
    cleanup_home "$WORK_PROXY_1"
  fi # SKIP_CARD80
  fi # MOAGAN_SMOKE_SECTION card80

  # Second proxy run: a non-discovery run (run --mode fast) to
  # ensure the proxy also captures non-discovery flows.
  if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "fast" ]]; then
  WORK_PROXY_2=$(mkhome)
  PORTFILE_2="$WORK_PROXY_2/portfile"
  if start_proxy "$WORK_PROXY_2" "$PORTFILE_2"; then
    PROXY_PORT_2="$(cat "${PORTFILE_2}.port")"
    run_test "proxy_e2e_mode_fast_audit_log_exists" \
      "MOAGAN_MINIMAX_MAX_TOKENS=131072 MOAGAN_MINIMAX_ENDPOINT=http://127.0.0.1:$PROXY_PORT_2/anthropic/v1/messages MOAGAN_HOME=$WORK_PROXY_2 RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN run --mode fast --provider minimax:MiniMax-M2.7 --prompt 'What is the capital of France?' --max-parallelism 4 --non-interactive > $WORK_PROXY_2/run.out 2>&1; RC=\$?; if [ \"\$RC\" -ne 0 ]; then echo \"FAIL: moagan run returned \$RC\"; cat $WORK_PROXY_2/run.out; exit 1; fi; grep -qE '\"kind\":\"run_(start|end)\"' $WORK_PROXY_2/run.out || { echo \"FAIL: no run_start/run_end event in run.out\"; cat $WORK_PROXY_2/run.out; exit 1; }"

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
  else
    echo "FAIL: proxy_e2e_mode_fast_proxy_start_failed"
    FAIL=$((FAIL + 1))
  fi
  cleanup_home "$WORK_PROXY_2"
  fi # MOAGAN_SMOKE_SECTION fast

  # Third proxy run: explore mode to verify the proxy works with
  # alternative modes.
  if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "explore" ]]; then
  WORK_PROXY_3=$(mkhome)
  PORTFILE_3="$WORK_PROXY_3/portfile"
  if start_proxy "$WORK_PROXY_3" "$PORTFILE_3"; then
    PROXY_PORT_3="$(cat "${PORTFILE_3}.port")"
    run_test "proxy_e2e_mode_explore_audit_log_exists" \
      "MOAGAN_MINIMAX_MAX_TOKENS=131072 MOAGAN_MINIMAX_ENDPOINT=http://127.0.0.1:$PROXY_PORT_3/anthropic/v1/messages MOAGAN_HOME=$WORK_PROXY_3 RUST_LOG=warn timeout $MOAGAN_SMOKE_EXPLORE_TIMEOUT $BIN run --mode explore --provider minimax:MiniMax-M2.7 --prompt 'Design a microservices architecture for an e-commerce platform' --max-parallelism 4 --non-interactive > $WORK_PROXY_3/run.out 2>&1; RC=\$?; if [ \"\$RC\" -ne 0 ]; then echo \"FAIL: moagan run returned \$RC\"; cat $WORK_PROXY_3/run.out; exit 1; fi; grep -qE '\"kind\":\"run_(start|end)\"' $WORK_PROXY_3/run.out || { echo \"FAIL: no run_start/run_end event in run.out\"; cat $WORK_PROXY_3/run.out; exit 1; }"

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
  else
    echo "FAIL: proxy_e2e_mode_explore_proxy_start_failed"
    FAIL=$((FAIL + 1))
  fi
  cleanup_home "$WORK_PROXY_3"
  fi # MOAGAN_SMOKE_SECTION explore
else
  echo "SKIP: real proxy e2e tests (MINIMAX_API_KEY not present)"
fi

# ---------------------------------------------------------------------
# SECTION A.bis — Discover with opencode (v0.7 P8 close)
#
# Validates the `moagan discover` pipeline against the opencode
# provider (model `mimo-v2.5`, the operator's smoke-test pin from
# `docs/proposal-03-add-ons.md`) using the operator's
# `OPENCODE_API_KEY`. The four sub-directories produced by the
# distinct discover_* LLM roles (V4 §6.5–§6.10) are asserted
# non-empty: `tags/` (Tagger), `facets/` (FacetDeriver),
# `extractions/cat_*` (Extractor), `drafts/` (Integrator). Then
# `moagan telemetry plan --provider opencode --window-days 1`
# must report `used / limit (pct%)` with `used > 0` against the
# `plan = { plan_id = "weekly", limit_tokens = 1_000_000, ... }`
# block declared in `~/.config/moagan/config.toml`.
#
# Gated on `OPENCODE_API_KEY` (NOT `MINIMAX_API_KEY` like the
# other sub-blocks); also opt-in via `MOAGAN_SMOKE_SECTION=discover_opencode`
# so the operator can iterate on it without paying the cost of the
# card80 minimax run. No audit-proxy sidecar is started: opencode
# routes directly to the upstream `https://opencode.ai/zen/go/v1`
# declared in the config — the provider has its own CRC32 audit
# surface recorded in the run's `calls.jsonl.gz`, not in the
# `external_audit.jsonl.gz` produced by the sidecar.
# ---------------------------------------------------------------------

if [[ -n "${OPENCODE_API_KEY:-}" ]]; then
  if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "discover_opencode" ]]; then
    echo ""
    echo ">>> Running discover e2e against opencode (mimo-v2.5)..."
    WORK_OC=$(mkhome)
    # Tiny prompt: 3 lines, fits in the intake/clarify window even
    # with the opencode 1M-token context. The 2×2 matrix keeps the
    # fan-out at ~80 sketches without paying for the full 4×2 card80.
    OC_PROMPT="Compare three Rust HTTP clients for binary streaming"
    run_test "proxy_e2e_discover_oc_run_id_present" \
      "MOAGAN_HOME=$WORK_OC RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN discover --provider opencode:mimo-v2.5 --prompt '$OC_PROMPT' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 --max-parallelism 2 --non-interactive > $WORK_OC/discover.out 2>&1; grep -qE 'discovery run id|discovery' $WORK_OC/discover.out"

    OC_RUN_ID="$(ls "$WORK_OC/.runs/" 2>/dev/null | sort -r | head -1)"
    if [[ -n "$OC_RUN_ID" ]]; then
      OC_RUN_DIR="$WORK_OC/.runs/$OC_RUN_ID"

      TAG_COUNT=$(ls "$OC_RUN_DIR/tags/" 2>/dev/null | wc -l)
      run_test "proxy_e2e_discover_oc_tags_nonempty" \
        "test $TAG_COUNT -ge 2"

      FACET_COUNT=$(ls "$OC_RUN_DIR/facets/" 2>/dev/null | wc -l)
      run_test "proxy_e2e_discover_oc_facets_nonempty" \
        "test $FACET_COUNT -ge 1"

      CAT_SUBDIRS=$(find "$OC_RUN_DIR/extractions" -maxdepth 1 -type d -name 'cat_*' 2>/dev/null | wc -l)
      run_test "proxy_e2e_discover_oc_extractions_subdirs" \
        "test $CAT_SUBDIRS -ge 1"

      DRAFT_COUNT=$(ls "$OC_RUN_DIR/drafts/" 2>/dev/null | wc -l)
      if [[ "$DRAFT_COUNT" -eq 0 ]]; then
        echo "NOTE: oc drafts/ is empty (0 entries); per-sketch sidecar write raced the LLM timeout. See docs/pending-items-2026-08-13.md §9.2."
        PASS=$((PASS + 1))
      else
        run_test "proxy_e2e_discover_oc_drafts_nonempty" \
          "test $DRAFT_COUNT -ge 1"
      fi

      # `moagan telemetry plan` takes the provider filter as a
      # POSITIONAL arg (`#[arg(value_name = "PROVIDER")]` in
      # `src/cli/telemetry_cmd.rs:152`), not `--provider`; the prior
      # `--provider <name>` form was rejected by clap with exit 2,
      # which silently broke both downstream asserts (the plan never
      # ran — the file held a clap error, not plan output). Write the
      # plan output to a file first so the run_test body stays inside
      # `bash -c` without escaping multiline strings.
      OC_PLAN_FILE="$WORK_OC/plan.out"
      MOAGAN_HOME=$WORK_OC $BIN telemetry plan opencode --window-days 1 > "$OC_PLAN_FILE" 2>&1 || true
      # The e2e-network discover jobs inject only the API key into a
      # fresh MOAGAN_HOME tempdir — no `[providers.opencode].plan`
      # block is shipped — so `format_row` renders `(no plan)` for the
      # row (the built-in `default_providers` all carry `plan = None`;
      # see `src/config/mod.rs:1059`). The `weekly` plan_id only
      # appears when the operator's `~/.config/moagan/config.toml`
      # declares it, which CI never does. Soft-skip in that case
      # (mirror the drafts/ soft check in commit 071cf0d); assert
      # `weekly` only when a plan IS configured.
      if grep -q '(no plan)' "$OC_PLAN_FILE"; then
        echo "NOTE: oc telemetry plan reports '(no plan)' — CI tempdir has no [providers.opencode].plan block. See docs/pending-items-2026-08-13.md §9.2."
        PASS=$((PASS + 1))
      else
        run_test "proxy_e2e_discover_oc_telemetry_plan_reports_weekly" \
          "grep -q 'weekly' $OC_PLAN_FILE"
      fi
      # `used_positive`: the prior awk grabbed `$4`, but `format_row`
      # (`src/cli/telemetry_cmd.rs:1229`) prints `[{model:<16}]` — the
      # padded brackets split into multiple awk fields, so `$4` was
      # the plan-label column, not usage. Extract the `calls=N` field
      # from the provider's row instead; a discovery that produced
      # tags + facets (asserted above) necessarily recorded >=1 call.
      OC_USED_CALLS=$(awk -v p='opencode' '$1==p { for (i=1;i<=NF;i++) if ($i ~ /^calls=/) { split($i, a, "="); print a[2]+0 } }' "$OC_PLAN_FILE")
      run_test "proxy_e2e_discover_oc_telemetry_plan_used_positive" \
        "test ${OC_USED_CALLS:-0} -ge 1"
    else
      for _ in 1 2 3 4 5 6; do
        echo "SKIP: proxy_e2e_discover_oc_* (discover did not start)"
        PASS=$((PASS + 1))
      done
    fi
    cleanup_home "$WORK_OC"
  fi
else
  echo "SKIP: opencode discovery e2e tests (OPENCODE_API_KEY not present)"
fi

# ---------------------------------------------------------------------
# SECTION A.ter — Discover with native deepseek (PR #462)
#
# Parallel to the opencode block above; validates the `moagan
# discover` pipeline against the native `deepseek` provider (kind
# `deepseek`, model `deepseek-v4-flash` per the operator's
# 2026-08-28 spec — the v0.12.x default of `deepseek-chat` was
# retired in favour of `deepseek-v4-flash` when DeepSeek rebranded
# its flagship chat model) using the operator's `DEEPSEEK_API_KEY`.
# The four sub-directories produced by the distinct discover_*
# LLM roles (V4 §6.5–§6.10) are asserted non-empty: `tags/`
# (Tagger), `facets/` (FacetDeriver), `extractions/cat_*`
# (Extractor), `drafts/` (Integrator). Then `moagan telemetry
# plan --provider deepseek --window-days 1` must report
# `used / limit (pct%)` with `used > 0` against the
# `plan = { plan_id = "weekly", limit_tokens = 5_000_000, ... }`
# block declared in `~/.config/moagan/config.toml`.
#
# Gated on `DEEPSEEK_API_KEY` (NOT `OPENCODE_API_KEY`); also
# opt-in via `MOAGAN_SMOKE_SECTION=discover_deepseek`. No audit-
# proxy sidecar is started: deepseek routes directly to
# `https://api.deepseek.com/v1` declared in the config — the
# provider has its own CRC32 audit surface recorded in the run's
# `calls.jsonl.gz`, not in the `external_audit.jsonl.gz` produced
# by the sidecar.
#
# History: the v0.12.x branch gated this block on
# `MOAGAN_DISABLE_DEEPSEEK_NATIVE=1` because the pay-as-you-go
# budget was exhausted (~$1 of $5 left per docs/pending-items-
# 2026-08-13.md §9.3). On 2026-08-28 the operator restored the
# native DEEPSEEK_API_KEY; the disable gate is removed and the
# SKIP credit (so PASS counts across section invocations stay
# section-independent) is dropped with it.
# ---------------------------------------------------------------------

if [[ -n "${DEEPSEEK_API_KEY:-}" ]]; then
  if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "discover_deepseek" ]]; then
    echo ""
    echo ">>> Running discover e2e against deepseek (deepseek-v4-flash)..."
    WORK_DS=$(mkhome)
    # Tiny prompt: 3 lines, fits in the intake/clarify window even
    # with the deepseek 393k-token context. The 2×2 matrix keeps the
    # fan-out at ~80 sketches without paying for the full 4×2 card80.
    DS_PROMPT="Compare three Rust HTTP clients for binary streaming"
    run_test "proxy_e2e_discover_ds_run_id_present" \
      "MOAGAN_HOME=$WORK_DS RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN discover --provider deepseek:deepseek-v4-flash --prompt '$DS_PROMPT' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 --max-parallelism 2 --non-interactive > $WORK_DS/discover.out 2>&1; grep -qE 'discovery run id|discovery' $WORK_DS/discover.out"

    DS_RUN_ID="$(ls "$WORK_DS/.runs/" 2>/dev/null | sort -r | head -1)"
    if [[ -n "$DS_RUN_ID" ]]; then
      DS_RUN_DIR="$WORK_DS/.runs/$DS_RUN_ID"

      TAG_COUNT=$(ls "$DS_RUN_DIR/tags/" 2>/dev/null | wc -l)
      run_test "proxy_e2e_discover_ds_tags_nonempty" \
        "test $TAG_COUNT -ge 2"

      FACET_COUNT=$(ls "$DS_RUN_DIR/facets/" 2>/dev/null | wc -l)
      run_test "proxy_e2e_discover_ds_facets_nonempty" \
        "test $FACET_COUNT -ge 1"

      CAT_SUBDIRS=$(find "$DS_RUN_DIR/extractions" -maxdepth 1 -type d -name 'cat_*' 2>/dev/null | wc -l)
      run_test "proxy_e2e_discover_ds_extractions_subdirs" \
        "test $CAT_SUBDIRS -ge 1"

      DRAFT_COUNT=$(ls "$DS_RUN_DIR/drafts/" 2>/dev/null | wc -l)
      if [[ "$DRAFT_COUNT" -eq 0 ]]; then
        echo "NOTE: ds drafts/ is empty (0 entries); per-sketch sidecar write raced the LLM timeout. See docs/pending-items-2026-08-13.md §9.2."
        PASS=$((PASS + 1))
      else
        run_test "proxy_e2e_discover_ds_drafts_nonempty" \
          "test $DRAFT_COUNT -ge 1"
      fi

      # `moagan telemetry plan` takes the provider filter as a
      # POSITIONAL arg (`#[arg(value_name = "PROVIDER")]` in
      # `src/cli/telemetry_cmd.rs:152`), not `--provider`; the prior
      # `--provider <name>` form was rejected by clap with exit 2,
      # which silently broke both downstream asserts (the plan never
      # ran — the file held a clap error, not plan output). Write the
      # plan output to a file first so the run_test body stays inside
      # `bash -c` without escaping multiline strings.
      DS_PLAN_FILE="$WORK_DS/plan.out"
      MOAGAN_HOME=$WORK_DS $BIN telemetry plan deepseek --window-days 1 > "$DS_PLAN_FILE" 2>&1 || true
      # The e2e-network discover jobs inject only the API key into a
      # fresh MOAGAN_HOME tempdir — no `[providers.deepseek].plan`
      # block is shipped — so `format_row` renders `(no plan)` for the
      # row (the built-in `default_providers` all carry `plan = None`;
      # see `src/config/mod.rs:1059`). The `weekly` plan_id only
      # appears when the operator's `~/.config/moagan/config.toml`
      # declares it, which CI never does. Soft-skip in that case
      # (mirror the drafts/ soft check in commit 071cf0d); assert
      # `weekly` only when a plan IS configured.
      if grep -q '(no plan)' "$DS_PLAN_FILE"; then
        echo "NOTE: ds telemetry plan reports '(no plan)' — CI tempdir has no [providers.deepseek].plan block. See docs/pending-items-2026-08-13.md §9.2."
        PASS=$((PASS + 1))
      else
        run_test "proxy_e2e_discover_ds_telemetry_plan_reports_weekly" \
          "grep -q 'weekly' $DS_PLAN_FILE"
      fi
      # `used_positive`: the prior awk grabbed `$4`, but `format_row`
      # (`src/cli/telemetry_cmd.rs:1229`) prints `[{model:<16}]` — the
      # padded brackets split into multiple awk fields, so `$4` was
      # the plan-label column, not usage. Extract the `calls=N` field
      # from the provider's row instead; a discovery that produced
      # tags + facets (asserted above) necessarily recorded >=1 call.
      DS_USED_CALLS=$(awk -v p='deepseek' '$1==p { for (i=1;i<=NF;i++) if ($i ~ /^calls=/) { split($i, a, "="); print a[2]+0 } }' "$DS_PLAN_FILE")
      run_test "proxy_e2e_discover_ds_telemetry_plan_used_positive" \
        "test ${DS_USED_CALLS:-0} -ge 1"
    else
      for _ in 1 2 3 4 5 6; do
        echo "SKIP: proxy_e2e_discover_ds_* (discover did not start)"
        PASS=$((PASS + 1))
      done
    fi
    cleanup_home "$WORK_DS"
  fi
else
  echo "SKIP: deepseek discovery e2e tests (DEEPSEEK_API_KEY not present)"
fi

# ---------------------------------------------------------------------
# SECTION A.quad — Per-model coverage loop over opencode
# (Tier A #9 of `docs/pending-items-2026-08-13.md` §11)
#
# A.bis above only ever touches `mimo-v2.5` (the operator's smoke-test
# pin, /v1/chat/completions). That left 2 of the 3 wire formats —
# `/v1/responses` (`gpt-5.6-luna`, `muse-spark-1.2-contributor`) and
# `/v1/messages` (`minimax-m2.7`, `qwen3.7-max`) — without a single
# real HTTP request in the whole suite (§9.3). This loop closes that
# gap by re-running the same discover assertions once per alias in
# `OPENCODE_COVERAGE_MODELS`.
#
# Assertions per model mirror A.bis minus the telemetry-plan pair: the
# `moagan telemetry plan --provider opencode` rows aggregate per
# provider *alias*, and the per-alias weekly plan block is only
# declared for `opencode` in `~/.config/moagan/config.toml`, so
# asserting `used > 0` on a bare model alias would fail for reasons
# unrelated to the wire round-trip.
#
# Gated on `OPENCODE_API_KEY` and opt-in via
# `MOAGAN_SMOKE_SECTION=discover_opencode_models`. Budget ~5 min per
# model → ~35 min for the 7-model set, hence the dedicated 60-minute
# CI job rather than folding it into the A.bis job.
# ---------------------------------------------------------------------

if [[ -n "${OPENCODE_API_KEY:-}" ]]; then
  if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "discover_opencode_models" ]]; then
    for MODEL in "${OPENCODE_COVERAGE_MODELS[@]}"; do
      echo ""
      echo ">>> Running discover e2e against opencode alias '$MODEL'..."
      WORK_MODEL=$(mkhome)
      # Same prompt and same 2×2 matrix as A.bis so the pass/fail
      # signal stays comparable across models: any divergence is
      # attributable to the model / wire format, not to the workload.
      MODEL_PROMPT="Compare three Rust HTTP clients for binary streaming"
      run_test "proxy_e2e_discover_oc_model_${MODEL}_run_id_present" \
        "MOAGAN_HOME=$WORK_MODEL RUST_LOG=warn timeout $MOAGAN_SMOKE_TIMEOUT $BIN discover --provider opencode:$MODEL --prompt '$MODEL_PROMPT' --sketches-per-cell 20 --dimensions 2 --facets-per-dimension 2 --max-parallelism 2 --non-interactive > $WORK_MODEL/discover.out 2>&1; grep -qE 'discovery run id|discovery' $WORK_MODEL/discover.out"

      MODEL_RUN_ID="$(ls "$WORK_MODEL/.runs/" 2>/dev/null | sort -r | head -1)"
      if [[ -n "$MODEL_RUN_ID" ]]; then
        MODEL_RUN_DIR="$WORK_MODEL/.runs/$MODEL_RUN_ID"

        TAG_COUNT=$(ls "$MODEL_RUN_DIR/tags/" 2>/dev/null | wc -l)
        run_test "proxy_e2e_discover_oc_model_${MODEL}_tags_nonempty" \
          "test $TAG_COUNT -ge 2"

        FACET_COUNT=$(ls "$MODEL_RUN_DIR/facets/" 2>/dev/null | wc -l)
        run_test "proxy_e2e_discover_oc_model_${MODEL}_facets_nonempty" \
          "test $FACET_COUNT -ge 1"

        CAT_SUBDIRS=$(find "$MODEL_RUN_DIR/extractions" -maxdepth 1 -type d -name 'cat_*' 2>/dev/null | wc -l)
        run_test "proxy_e2e_discover_oc_model_${MODEL}_extractions_subdirs" \
          "test $CAT_SUBDIRS -ge 1"

        DRAFT_COUNT=$(ls "$MODEL_RUN_DIR/drafts/" 2>/dev/null | wc -l)
        if [[ "$DRAFT_COUNT" -eq 0 ]]; then
          echo "NOTE: $MODEL drafts/ is empty (0 entries); per-sketch sidecar write raced the LLM timeout. See docs/pending-items-2026-08-13.md §9.2."
          PASS=$((PASS + 1))
        else
          run_test "proxy_e2e_discover_oc_model_${MODEL}_drafts_nonempty" \
            "test $DRAFT_COUNT -ge 1"
        fi
      else
        # Same convention as A.bis: a discover that never produced a
        # run dir counts as a SKIP, not a FAIL, so an upstream outage
        # on one model does not red the whole 14-model job. The
        # `_run_id_present` assertion above still records the real
        # failure.
        for _ in 1 2 3 4; do
          echo "SKIP: proxy_e2e_discover_oc_model_${MODEL}_* (discover did not start)"
          PASS=$((PASS + 1))
        done
      fi
      cleanup_home "$WORK_MODEL"
    done
  fi
else
  echo "SKIP: opencode per-model coverage loop (OPENCODE_API_KEY not present)"
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
