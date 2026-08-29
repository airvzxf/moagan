# Test skips inventory — moagan

A complete catalogue of every place the test suite skips code on
purpose. Use this when:

- A PR fails CI on a test you "didn't touch" — check if it's in this list.
- Adding a new skip — confirm it's not already covered by an existing mechanism.
- Removing a skip — verify the test now passes reliably on cold cache (locally
  with `cargo clean -p moagan`).

## Layer 1 — Ruleset `protect-main` (GitHub branch rules)

The ruleset protects `main` via `gh api /repos/airvzxf/moagan/rulesets/19743104`.

It has **6 rules** (`deletion`, `non_fast_forward`, `pull_request`,
`required_linear_history`, `required_status_checks`,
`required_signatures`). None of them "skip"
a check per se; they require what to pass.

The closest thing to a skip is **`required_status_checks.contexts`** — the
list of CI jobs that MUST be green before merge. Anything not on the
list is implicitly **not enforced**.

| Job | Required? | Notes |
|---|---|---|
| `T0 · fmt-check` | ✅ required | |
| `T0 · guard-deps` | ✅ required | |
| `T1 · clippy` | ✅ required | |
| `T2 · cargo test --lib --bins` | ✅ required | |
| `T2 · cargo test --tests (integration)` | ✅ required | |
| `T2 · cargo test --doc` | ✅ required | |
| `T3 · make smoke` | ✅ required | |
| `T3 · make e2e (local mock pipeline)` | ✅ required | |
| `e2e-network` workflow (post-merge) | ❌ **not required** | Runs only on push to main; not a merge gate |

Plus the ruleset-level `required_signatures` rule, which enforces
GPG signing on every commit landing on `main` (the same invariant
that `commit.gpgsign=true` enforces locally; the ruleset entry is
the last-resort guard so a bypass at the local hook still gets
caught at the ruleset gate). See
[`docs/branch-protection.md`](branch-protection.md) for the full
`gh api` block.

To update these, edit the ruleset via `gh api` (see
`docs/branch-protection.md` for the PUT block).

## Layer 2 — `cargo test --skip` (CLI-level test exclusions)

**Empty as of 2026-08-07.** All 8 entries previously listed here
were shipped between PRs #229, #230, #234, #237 (May–Aug 2026) to
work around CI cold-cache flakiness. The 8 entries were closed by
PRs #240, #242, #244, #246, #248 (Aug 2026), each addressing a
distinct root cause:

| Test | Closed by | Root cause |
|---|---|---|
| `cli::diff::tests::diff_unknown_run_returns_invalid_state` | #240 | `MOAGAN_HOME` env var race; test was not using the `home_override` helper that PR #129 added to `DiffArgs` |
| `validators::rust_validator::tests::*` (×5) | #242 | Sandbox overrides `HOME` per-invocation, so the cargo `--offline` invocation needed `CARGO_HOME` prewarmed into a `OnceLock` keyed on `MOAGAN_HOME` |
| `audit_e2e_deep_run_has_exact_external_coverage` | #244 | Credit-card redaction regex `\b(?:\d[ -]?){13,16}\b` redacted a small fraction (≈0.015%) of UUID v7 `call_id`s, so the in-process verify count diverged from the SQLite cross-check |
| `llm::response_format_opt_out::tests::env_var_extends_opt_out` | #246 | `ENV_LOCK` only wrapped `set_var` / `remove_var`; the asserts in between ran in the unlocked gap and raced with parallel tests reading the env |

**Total: 0 tests currently skipped via `--skip` CLI flag.**

The skip list is empty. The historical three skip sites
(`Makefile::test-ci`, `ci.yml::test-lib`, `ci.yml::test-tests`)
have been collapsed back to plain `cargo test` invocations:

- `Makefile:72-73` — `test-ci` runs `MOAGAN_NON_INTERACTIVE=1 cargo test --all-targets`
- `.github/workflows/ci.yml::test-lib` — runs `cargo test --lib --bins`
- `.github/workflows/ci.yml::test-tests` — runs `cargo test --tests --no-fail-fast`

If a future test ever needs to be skipped again, follow the
"How to add a new skip" playbook below. Until then, every test
listed under Layer 3–6 is the only kind of skip on the suite.

## Layer 3 — `#[ignore]` attribute (Rust source-level)

`#[ignore]` tests are **compiled but not run** by default. They run
when invoked with `cargo test -- --ignored` or `cargo test <name> -- --ignored`.

| Test | File | Reason |
|---|---|---|
| `prlimit_apply_sets_nproc_rlimit` | `src/sandbox/cgroup.rs:465` | Mutates process-wide RLIMIT_NPROC (side-effects other tests) |
| `prlimit_apply_sets_as_rlimit` | `src/sandbox/cgroup.rs:510` | Mutates process-wide RLIMIT_AS (side-effects other tests) |
| `audit_e2e_deep_run_has_exact_external_coverage` | `tests/integration_audit_e2e.rs:259` | Known-flaky under parallel execution (documented as such in `AGENTS.md`); only runnable via `cargo test -- --ignored` (the test is `#[ignore]`d and `make e2e-network` does not auto-invoke it) |
| `discover_opencode_writes_four_subdirs` | `tests/integration_discover_opencode.rs:37` | Requires `OPENCODE_API_KEY`; only runs locally / in `e2e-network` |
| `discover_deepseek_writes_four_subdirs` | `tests/integration_discover_deepseek.rs:37` | Requires `DEEPSEEK_API_KEY`; only runs locally / in `e2e-network` |
| `discover_minimax_writes_four_subdirs` | `tests/integration_discover_minimax.rs:40` | Requires `MINIMAX_API_KEY`; only runs locally / in `e2e-network` |

Total: **6 tests marked `#[ignore]`**.

Note: these are NOT included in `cargo test --skip`. To run them:

```bash
cargo test --lib -- --ignored
```

## Layer 4 — Source-level silent skips (binary missing on PATH)

Tests that exit early via `return;` when a required external tool is
missing from `$PATH`. The test runs but produces no signal (not even
a pass). These are NOT exposed via `--skip` (cargo doesn't know about
them).

| Validator | Tests that silently skip | Required binary |
|---|---|---|
| `rust_validator` | `good_rust_passes_when_cargo_present`, `broken_rust_fails_when_cargo_present`, `broken_rust_skips_remaining_steps_after_check_failure`, `good_rust_runs_three_steps_in_order`, `rust_validator_fails_when_test_fails`, `rust_validator_marks_no_tests_as_skipped`, `rust_validator_passes_when_test_passes` | `cargo` |
| `typescript_validator` | `good_ts_passes_when_tsc_present`, `broken_ts_fails_when_tsc_present` | `tsc` |
| `python_validator` | `good_python_passes_when_python_present`, `broken_python_fails_when_python_present` | `python3` |
| `sql_validator` | `sqlite_engine_passes_on_valid_select`, `sqlite_engine_fails_on_broken_select` | `sqlite3` |

Pattern (from `src/validators/rust_validator.rs:558`, the first
silent-skip site for `cargo`; the same shape repeats at
`src/validators/python_validator.rs:235` for `python3` and at
`src/validators/typescript_validator.rs:241` for `tsc`):

```rust
if std::process::Command::new("cargo").arg("--version").output().is_err() {
    return; // skip silently
}
```

Total: **13 tests with silent-skip pattern**. None are blocked by
CI (the runners have all 4 binaries). Locally, `cargo`, `tsc`,
`python3`, and `sqlite3` may or may not be present; missing ones
silently no-op.

## Layer 5 — `ValidationEvidence::skipped()` (per-artifact runtime skips)

The Rust validators return `Ok(ValidationEvidence::skipped(name, reason))`
when an individual artifact doesn't qualify for that validator. This
is **runtime behavior**, not a test skip; the validator still runs
but reports `Skipped` instead of `Pass`/`Fail`.

| File | Line | Reason |
|---|---|---|
| `src/validators/python_validator.rs` | 49 | Source doesn't look like Python |
| `src/validators/python_validator.rs` | 85 | Per-artifact check skipped (Validator trait default) |
| `src/validators/rust_validator.rs` | 73 | Source doesn't look like Rust |
| `src/validators/rust_validator.rs` | 224 | Per-artifact check skipped (Validator trait default) |
| `src/validators/typescript_validator.rs` | 51 | Source doesn't look like TypeScript |
| `src/validators/typescript_validator.rs` | 85 | Per-artifact check skipped |
| `src/validators/schema_validator.rs` | 75 | No schema to validate |
| `src/validators/schema_validator.rs` | 146 | Per-artifact check skipped |
| `src/validators/sql_validator.rs` | 100 | No SQL detected |
| `src/validators/sql_validator.rs` | 113 | Source splits into zero SQL statements (per-statement split on `;`) |
| `src/validators/sql_validator.rs` | 215 | Per-artifact check skipped |

Total: **11 runtime `skipped` returns**. Each is a normal code path,
not a test exclusion.

## Layer 6 — Bash script conditional runs

The e2e proxy suite (`scripts/e2e_audit_proxy.sh`) has **69**
`run_test` invocations in total (counted via
`grep -c "run_test" scripts/e2e_audit_proxy.sh`); only the
subset that runs in a default `make e2e-network` invocation
is documented below. The MINIMAX_API_KEY-gated block (6a)
contributes 46; the OPENCODE_API_KEY-gated discover block
(6d) contributes 7 (+ 1 OC_RUN_ID skip fallback); the
DEEPSEEK_API_KEY-gated discover block (6e) contributes 7
(+ 1 DS_RUN_ID skip fallback); the MOAGAN_SMOKE_SECTION-gated
discover_opencode_models block (6g) contributes 5 per model
× 7 models = 35; the gauntlet.sh `MINIMAX_API_KEY` re-check
(6f) contributes 1. The remaining ~6 `run_test` invocations
in the script are for upstream-side assertions (per-run audits,
post-run health checks, the proxy-e2e `moagan run` mode
block) that are not currently gated on a secret and always
run on `make e2e-network`.

Several of the gated blocks are conditionally executed:

### 6a. Real proxy e2e tests skipped when `MINIMAX_API_KEY` is missing

```bash
if [[ -n "${MINIMAX_API_KEY:-}" ]]; then
  # ~46 run_test calls here
else
  echo "SKIP: real proxy e2e tests (MINIMAX_API_KEY not present)"
fi
```

When the API key is absent, **all 46 real-proxy tests are skipped**
(printed as a single SKIP line). On CI runners the key IS present
(via `secrets.MINIMAX_API_KEY`), so this never triggers.

### 6b. card80 block skipped via `MOAGAN_SMOKE_LONG_DISCOVER=1`

```bash
if [[ "$MOAGAN_SMOKE_LONG_DISCOVER" == "1" ]]; then
  echo "SKIP: proxy_e2e_card80_* (MOAGAN_SMOKE_LONG_DISCOVER=1)"
  PASS=$((PASS + 37))   # 37 test invocations counted as pass
fi
```

Set this env var to skip the 25-minute `card80` discovery block.
**37 tests in the card80 block.**

### 6c. card80 partial skips when discovery didn't complete

Inside the card80 block, if the discover phase didn't finish in time,
specific subgroups are skipped (instead of failing) and counted as
PASS to keep totals consistent:

| Subgroup | Skip trigger | Counted as |
|---|---|---|
| `proxy_e2e_card80_tags_*` | `tags/` dir has < 2 files | 3 PASS |
| `proxy_e2e_card80_clusters_*` | `clusters/` dir has < 2 dirs | 2 PASS |
| `proxy_e2e_card80_facets_present` | `facets/` dir missing | 1 PASS |
| `proxy_e2e_card80_extractions_subdirs_present` | no `cat_*` subdirs | 1 PASS |
| `proxy_e2e_card80_summary_*` (×7) | `final/summary.md` missing | 7 PASS |

Total: 14 of the 37 card80 tests have a partial-skip path.

### 6d. `OPENCODE_API_KEY` → 7 discover_oc tests (PR #460)

```bash
if [[ -n "${OPENCODE_API_KEY:-}" ]]; then
  # 8 run_test calls inside the opencode discover block
  # (the `opencode` section was the v0.10 alias for the
  # historical `opencode_go` section, finalized in v0.13.x)
else
  echo "SKIP: opencode discovery e2e tests (OPENCODE_API_KEY not present)"
fi
```

- **Location:** `scripts/e2e_audit_proxy.sh:580–662` (the
  `discover_opencode` block, conditional on `OPENCODE_API_KEY`).
- **Tests:** 7 `run_test` invocations + 1 `OC_RUN_ID` skip fallback
  (`proxy_e2e_discover_oc_run_id_present`,
  `proxy_e2e_discover_oc_tags_nonempty`,
  `proxy_e2e_discover_oc_facets_nonempty`,
  `proxy_e2e_discover_oc_extractions_subdirs`,
  `proxy_e2e_discover_oc_drafts_nonempty`,
  `proxy_e2e_discover_oc_telemetry_plan_reports_weekly`,
  `proxy_e2e_discover_oc_telemetry_plan_used_positive`,
  plus the `OC_RUN_ID` skip fallback that counts each missing subdir as
  one PASS).
- **CI behaviour:** the `OPENCODE_API_KEY` secret IS registered on
  the runner (alongside `DEEPSEEK_API_KEY`), but the auto-triggered
  `e2e-network.yml` no longer consumes it (the discover-heavy path
  moved to the manual-only `e2e-network-discover-opencode.yml` /
  `e2e-network-discover-opencode-models.yml` workflows). On
  `make e2e-network` runs, all 8 tests print a single `SKIP` line
  because the env var `OPENCODE_API_KEY` is not set in the
  default local-dev shell; only the manual discover workflows
  (which set the key in their own `secrets:` block) actually
  exercise it. Pair this block with `Layer 3`'s
  `discover_opencode_writes_four_subdirs` `#[ignore]` integration
  test, which is also gated on the same secret.

### 6e. `DEEPSEEK_API_KEY` → 7 discover_ds tests (PR #462)

```bash
if [[ -n "${DEEPSEEK_API_KEY:-}" ]]; then
  # 8 run_test calls inside the deepseek discover block
else
  echo "SKIP: deepseek discovery e2e tests (DEEPSEEK_API_KEY not present)"
fi
```

- **Location:** `scripts/e2e_audit_proxy.sh:683–781` (the
  `discover_deepseek` block, conditional on `DEEPSEEK_API_KEY`).
- **Tests:** 7 `run_test` invocations + 1 `DS_RUN_ID` skip fallback —
  parallel structure to 6d
  (`proxy_e2e_discover_ds_{run_id_present,tags_nonempty,facets_nonempty,extractions_subdirs,drafts_nonempty,telemetry_plan_reports_weekly,telemetry_plan_used_positive}`).
- **CI behaviour:** the `DEEPSEEK_API_KEY` secret IS registered on
  the runner, but the auto-triggered `e2e-network.yml` no longer
  consumes it (the discover path moved to the manual-only
  `e2e-network-discover-deepseek.yml` workflow). On `make
  e2e-network` runs, all 8 tests print a single `SKIP` line. Pair
  with `Layer 3`'s `discover_deepseek_writes_four_subdirs`
  `#[ignore]` integration test.

### 6f. `MINIMAX_API_KEY` re-check inside `scripts/gauntlet.sh` (1 test)

```bash
if [[ -n "${MINIMAX_API_KEY:-}" ]]; then
  run_gate "moagan run --mode fast --provider minimax:MiniMax-M3" \
    bash -c "$BIN run --mode fast --provider minimax:MiniMax-M3 ..."
else
  skip_gate "moagan run --mode fast --provider minimax:MiniMax-M3" "MINIMAX_API_KEY not set"
fi
```

- **Location:** `scripts/gauntlet.sh:143` (one `run_gate` invocation
  wrapped in an `MINIMAX_API_KEY` check; the same `if/else` at
  line 149 prints a `skip_gate` line with that label).
- **Tests:** 1 (`moagan run --mode fast --provider minimax:MiniMax-M3`).
- **CI behaviour:** the key is registered as `secrets.MINIMAX_API_KEY`
  on the runner, so this branch normally fires; the `else` only
  triggers for local developer runs without a key. Distinct from
  Layer 6a (which guards the e2e proxy block in
  `e2e_audit_proxy.sh`) — both gates exist independently.

### 6g. `MOAGAN_SMOKE_SECTION=discover_opencode_models` → 7-model sweep (manual)

```bash
if [[ "$MOAGAN_SMOKE_SECTION" == "all" || "$MOAGAN_SMOKE_SECTION" == "discover_opencode_models" ]]; then
  for MODEL in "${OPENCODE_COVERAGE_MODELS[@]}"; do
    # 5 run_test calls per model
  done
else
  echo "SKIP: 7-model opencode sweep (MOAGAN_SMOKE_SECTION=$MOAGAN_SMOKE_SECTION)"
fi
```

- **Location:** `scripts/e2e_audit_proxy.sh:803+` (the
  `discover_opencode_models` block, conditional on
  `MOAGAN_SMOKE_SECTION`). Cost ~5 min per model × 7 = ~35 min
  total.
- **Tests:** 5 `run_test` invocations per model × 7 models = **35
  total**, mirroring the opencode discover block: per model the
  script asserts `proxy_e2e_discover_oc_model_${MODEL}_run_id_present`,
  `_tags_nonempty`, `_facets_nonempty`, `_extractions_subdirs`, and
  `_drafts_nonempty`.
- **Models:** per `OPENCODE_COVERAGE_MODELS` array in
  `scripts/e2e_audit_proxy.sh:122-130`:
  - `deepseek-v4-flash` (opencode alias, distinct from native
    `deepseek` provider)
  - `glm-5.3-flash`
  - `gpt-5.6-luna`
  - `mimo-v2.5` (also the smoke-test model in A.bis)
  - `minimax-m2.7`
  - `muse-spark-1.2-contributor`
  - `qwen3.7-max`
- **CI behaviour:** the block is **double-gated** on both
  `MOAGAN_SMOKE_SECTION` and `OPENCODE_API_KEY` (the latter is
  checked by the inner `moagan discover --provider opencode:$MODEL`
  invocations, which fail to start without a key). On a default
  `make e2e-network` run, the block SKIPs unless the operator
  explicitly passes `MOAGAN_SMOKE_SECTION=discover_opencode_models`
  AND has `OPENCODE_API_KEY` set. The CI workflow never sets
  either, so the block is dormant in CI.

## Layer 7 — Lefthook escape hatches (developer-side)

`lefthook.yml` doesn't skip any test by default. It offers three
**escape hatches** that bypass hooks (the opposite of skip — they
let the dev skip the validation entirely):

| Escape hatch | Effect |
|---|---|
| `LEFTPHOOK=0 git commit -m "..."` | Disable all lefthook hooks for one command |
| `git commit --no-verify` | Bypass pre-commit + commit-msg |
| `git push --no-verify` | Bypass pre-push (T2 cargo test still runs in CI) |

These do NOT affect CI — only local development. CI re-runs the full
check from a clean state.

## Summary table

| Layer | Mechanism | Count | Auto-skipped on CI? |
|---|---|---|---|
| 1 | Ruleset required_status_checks | 8 jobs required, 1 not | n/a |
| 2 | `cargo test --skip` CLI flag | 0 tests | n/a (closed) |
| 3 | `#[ignore]` Rust attribute | 6 tests | ❌ no (run via `--ignored`) |
| 4 | Source silent-skip (binary on PATH) | 13 tests | ✅ partially (binaries present) |
| 5 | `ValidationEvidence::skipped()` runtime | 11 sites | n/a (per-artifact) |
| 6a | `MINIMAX_API_KEY` missing → 46 tests | 46 tests | ❌ no (key present in CI) |
| 6b | `MOAGAN_SMOKE_LONG_DISCOVER=1` | 37 tests | ❌ no (env var unset) |
| 6c | card80 partial-skips on timeout | 14 conditional | conditional |
| 6d | `OPENCODE_API_KEY` missing → 7 tests + 1 OC fallback | 8 tests | ❌ no (key registered, not consumed in e2e-network) |
| 6e | `DEEPSEEK_API_KEY` missing → 7 tests + 1 DS fallback | 8 tests | ❌ no (key registered, not consumed in e2e-network) |
| 6f | `MINIMAX_API_KEY` in `gauntlet.sh:143` | 1 test | ❌ no (key present in CI) |
| 6g | `MOAGAN_SMOKE_SECTION=discover_opencode_models` + `OPENCODE_API_KEY` (manual) | 35 tests (5 × 7 models) | ❌ no (env var + key never set in CI) |
| 7 | lefthook escape hatches | n/a (escape hatches) | ❌ no |

**Total tests actively skipped on CI:** 0.
**Total tests in conditional skip code paths:** 63
(46 `MINIMAX_API_KEY` (6a, the entire real-proxy block from
`if [[ -n "${MINIMAX_API_KEY` to the closing `fi` of the
SECTION A.bis) + 7 OPENCODE + 1 OC_RUN_ID fallback (6d) + 7
DEEPSEEK + 1 DS_RUN_ID fallback (6e) + 1 gauntlet `MINIMAX_API_KEY`
re-check (6f); the card80 37-test block (6b) and the 14
partial-skips (6c) are subsets of the 6a total, not additions.
The 6g block (35 tests, 5 × 7 models) is not in this total
because it is dormant in CI by design (`MOAGAN_SMOKE_SECTION`
is never set in the workflow)).

## How to add a new skip

> **Note (2026-08-07):** Layer 2 is currently empty. The playbook
> below is kept as the reference for the next skip, if one is ever
> needed. **Do not add a `--skip` entry without first attempting the
> "remove the root cause" path from `How to remove a skip` below** —
> the eight skips that lived here from May–Aug 2026 were all root-
> cause-fixed and removed, so the bar for adding a new one is now
> "all other options exhausted".

1. **If the test is flaky in CI cold cache:** add it to the three
   skip sites:

   - `Makefile::test-ci` target
   - `.github/workflows/ci.yml::test-lib` job
   - `.github/workflows/ci.yml::test-tests` job

   Add the test name with `^name::exact_match$` so `cargo test
   --skip <name>` does not accidentally match other tests. Document
   the root cause and the planned removal PR in the commit body,
   and add a row to the Layer 2 table above.

2. **If the test mutates global state:** use `#[ignore]` in source.
   Add a clear `#[ignore = "reason"]` message.

3. **If the test depends on an external binary that's not always
   available:** use the silent-skip pattern (return early if binary
   missing). Document the binary in Layer 4 above.

4. **If the test depends on secrets (e.g., API key):** wrap in an
   `if [[ -n "${KEY:-}" ]]; then ... fi` block. Print a SKIP line
   in the `else` branch.

## How to remove a skip

1. **For `--skip` flag:** run the test locally with `cargo test
   <name> -- --exact --nocapture` (without `--skip`) several times.
   If it passes consistently, remove from all 3 skip sites:

   - `Makefile::test-ci`
   - `.github/workflows/ci.yml::test-lib`
   - `.github/workflows/ci.yml::test-tests`

   Then `grep -rn -- --skip` to confirm no stale references
   remain. Update the Layer 2 row in this doc.

2. **For `#[ignore]`:** the test must pass reliably when run
   normally. Use `cargo test <name> -- --ignored` to verify.

3. **For silent-skip:** ensure the binary IS available on the CI
   runner (check the runner image docs at
   `https://github.com/actions/runner-images`).

4. **For conditional (API key, env var):** remove the `if` wrapper
   and let the test always run. Update the docs.

### Removed skips (Aug 2026 cleanup)

All 8 entries in Layer 2 were closed by PRs #240, #242, #244, #246,
#248. The winning pattern: diagnose the actual root cause, fix the
test setup (helper, fixture, or call pattern) rather than the test
assertion, stress-run with `--test-threads=4`, then remove from all
three skip sites (Makefile + ci.yml × 2 jobs) and `grep -rn -- --skip`
to confirm no stale references remain. Layer 2 has been empty since
2026-08-07.
