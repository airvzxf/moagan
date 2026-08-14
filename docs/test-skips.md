# Test skips inventory — moagan

A complete catalogue of every place the test suite skips code on
purpose. Use this when:

- A PR fails CI on a test you "didn't touch" — check if it's in this list.
- Adding a new skip — confirm it's not already covered by an existing mechanism.
- Removing a skip — verify the test now passes reliably on cold cache (locally
  with `cargo clean -p moagan`).

## Layer 1 — Ruleset `protect-main` (GitHub branch rules)

The ruleset protects `main` via `gh api /repos/airvzxf/moagan/rulesets/19743104`.

It has **5 rules** (`deletion`, `non_fast_forward`, `pull_request`,
`required_linear_history`, `required_status_checks`). None of them "skip"
a check per se; they require what to pass.

The closest thing to a skip is **`required_status_checks.contexts`** — the
list of CI jobs that MUST be green before merge. Anything not on the
list is implicitly **not enforced**.

| Job | Required? | Notes |
|---|---|---|
| `T0 · fmt-check` | ✅ required | |
| `T0 · guard-deps` | ✅ required | |
| `T1 · clippy` | ✅ required | |
| `T1 · build (populates cargo cache)` | ✅ required | |
| `T2 · cargo test --lib --bins` | ✅ required | |
| `T2 · cargo test --tests (integration)` | ✅ required | |
| `T2 · cargo test --doc` | ✅ required | |
| `T3 · make smoke` | ✅ required | |
| `T3 · make e2e (local mock pipeline)` | ✅ required | |
| `e2e-network` workflow (post-merge) | ❌ **not required** | Runs only on push to main; not a merge gate |

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

- `Makefile:65-66` — `test-ci` runs `cargo test --all-targets`
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
| `prlimit_apply_sets_nproc_rlimit` | `src/sandbox/cgroup.rs:396` | Mutates process-wide RLIMIT_NPROC (side-effects other tests) |
| `prlimit_apply_sets_as_rlimit` | `src/sandbox/cgroup.rs:441` | Mutates process-wide RLIMIT_AS (side-effects other tests) |
| `audit_e2e_deep_run_has_exact_external_coverage` | `tests/integration_audit_e2e.rs:259` | Known-flaky under parallel execution (documented as such in `AGENTS.md`); exercised by `make e2e-network` |
| `discover_opencode_go_writes_four_subdirs` | `tests/integration_discover_opencode_go.rs:32` | Requires `OPENCODE_GO_API_KEY`; only runs locally / in `e2e-network` |
| `discover_deepseek_writes_four_subdirs` | `tests/integration_discover_deepseek.rs:32` | Requires `DEEPSEEK_API_KEY`; only runs locally / in `e2e-network` |

Total: **5 tests marked `#[ignore]`**.

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

Pattern (from `src/validators/rust_validator.rs:407`):

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
| `src/validators/python_validator.rs` | 44 | Source doesn't look like Python |
| `src/validators/python_validator.rs` | 76 | Per-artifact check skipped (Validator trait default) |
| `src/validators/rust_validator.rs` | 68 | Source doesn't look like Rust |
| `src/validators/rust_validator.rs` | 214 | Per-artifact check skipped (Validator trait default) |
| `src/validators/typescript_validator.rs` | 44 | Source doesn't look like TypeScript |
| `src/validators/typescript_validator.rs` | 74 | Per-artifact check skipped |
| `src/validators/schema_validator.rs` | 68 | No schema to validate |
| `src/validators/schema_validator.rs` | 133 | Per-artifact check skipped |
| `src/validators/sql_validator.rs` | 90 | No SQL detected |
| `src/validators/sql_validator.rs` | 102 | SQLite binary missing on PATH |
| `src/validators/sql_validator.rs` | 182 | Per-artifact check skipped |

Total: **11 runtime `skipped` returns**. Each is a normal code path,
not a test exclusion.

## Layer 6 — Bash script conditional runs

The e2e proxy suite (`scripts/e2e_audit_proxy.sh`) has 46
`run_test` calls, but several are conditionally executed:

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
| 1 | Ruleset required_status_checks | 9 jobs required, 1 not | n/a |
| 2 | `cargo test --skip` CLI flag | 0 tests | n/a (closed) |
| 3 | `#[ignore]` Rust attribute | 5 tests | ❌ no (run via `--ignored`) |
| 4 | Source silent-skip (binary on PATH) | 13 tests | ✅ partially (binaries present) |
| 5 | `ValidationEvidence::skipped()` runtime | 11 sites | n/a (per-artifact) |
| 6a | `MINIMAX_API_KEY` missing → 46 tests | 46 tests | ❌ no (key present in CI) |
| 6b | `MOAGAN_SMOKE_LONG_DISCOVER=1` | 37 tests | ❌ no (env var unset) |
| 6c | card80 partial-skips on timeout | 14 conditional | conditional |
| 7 | lefthook escape hatches | n/a (escape hatches) | ❌ no |

**Total tests actively skipped on CI:** 0.
**Total tests in conditional skip code paths:** 60 (card80 subgroups + API-key block).

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

The 8 entries in Layer 2 were all closed in a single cleanup
session (#240, #242, #244, #246, #248). The pattern that worked:

1. **Diagnose** the actual root cause (NOT just patch the test or
   mute the symptom). Three of the four PRs discovered the
   orchestrator's predicted root cause was wrong:

   - The mutex fix (PR #238) was incomplete — the `ENV_LOCK` only
     wrapped `set_var` / `remove_var`, not the asserts in between
     (PR #246 discovered this and folded the two `with_lock`
     blocks into one that covers the entire test body).
   - The "prewarm cargo" diagnosis was correct, but the
     implementation had to thread `CARGO_HOME` through the
     sandbox env (PR #242 discovered the sandbox overrides
     `HOME` per-invocation, which silently shadowed the
     prewarmed target dir).
   - The redaction regex catches 16-digit UUIDs and a few
     credit-card-shaped decimals; the audit verify was
     in-process while the SQLite cross-check was not
     (PR #244 discovered the divergence path).

2. **Fix the source** in the test setup (helper, validator
   fixture, or call pattern), not the test assertion. The 8
   fixes are split roughly 50/50 between new helpers
   (`DiffArgs::home_override`, the `CARGO_HOME` `OnceLock`) and
   pattern rewrites (the merged `ENV_LOCK` scope, the
   in-process verify bypass).

3. **Re-validate** with a stress run (20+ invocations under
   `--test-threads=4`) before removing from the skip list. None
   of the four PRs removed the skip in the same commit as the
   fix; they landed the fix first, watched the skipped test pass
   for several CI runs, then landed a follow-up commit removing
   the skip entry.

4. **Remove from all 3 skip sites** (Makefile + ci.yml × 2
   jobs). PR squash-merges that "touched the file but did not
   delete the line" bit us twice — always `grep -rn -- --skip`
   after the merge lands.
