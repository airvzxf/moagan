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

- `Makefile:96` — `test` runs `MOAGAN_NON_INTERACTIVE=1 cargo test --all-targets`
- `Makefile:99` — `test-ci` runs `MOAGAN_NON_INTERACTIVE=1 cargo test --all-targets`
- `Makefile:102` — `test-doc` runs `MOAGAN_NON_INTERACTIVE=1 cargo test --doc`
- `.github/workflows/ci.yml:137` (`test-tests`) — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --tests --no-fail-fast` (env block on the step plus inline prefix inside the `bash -c '...'` heredoc)
- `.github/workflows/ci.yml:194` (`test-lib`) — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --lib --bins --no-fail-fast` (same defense-in-depth pattern)
- `.github/workflows/ci.yml:377` (`test-doc`) — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --doc` (same)
- `.github/workflows/test-ignored-minimax.yml:104` — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_discover_minimax -- --ignored --nocapture`
- `.github/workflows/test-ignored-deepseek.yml:108` — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_discover_deepseek -- --ignored --nocapture`
- `.github/workflows/test-ignored-opencode.yml:110` — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --test integration_discover_opencode -- --ignored --nocapture`
- `scripts/gauntlet.sh:114` — runs `MOAGAN_NON_INTERACTIVE=1 cargo test --all-targets`

Smoke scripts (`scripts/`) — all `MOAGAN_NON_INTERACTIVE=1 cargo test` invocations:

- `scripts/smoke_phase_f.sh:468,471,474,477,480,483,486,489,492,495` — SECTION 9 (full `--all-targets`, `--lib`, `--test integration_phase_d` runs)
- `scripts/smoke_phase_g.sh:108,112` — domain::tests problem-graph + Kahn lib tests
- `scripts/smoke_phase_h.sh:125,129,133,196,200,204,212,220,228,236,240,248` — ranking::stability, phases::propose, phases::rank, integration_phase_h batches
- `scripts/smoke_phase_k.sh:105,150,177,206,215,220` — embed, sqlite v008, redact, retry_budget, constraint, integration_phase_k
- `scripts/smoke_phase_l.sh:51` — `cargo test --manifest-path ... --test integration_phase_l`
- `scripts/smoke_phase_n.sh:94,98` — sandbox::process + integration_phase_n
- `scripts/smoke_phase_o.sh:80,84,92` — ranking::rubric, storage::compression, integration_phase_o
- `scripts/smoke_discovery.sh:640` — `--lib` test-count probe
- `scripts/smoke_audit_proxy.sh:733` — `--lib` test-count probe
- `scripts/smoke_circuit_breaker.sh:165,169,173,177,181` — circuit_breaker, llm::provider, error, config, integration_circuit_breaker
- `scripts/smoke_cancel_hard.sh:109,113,117` — cancel, sandbox::process, integration_cancel_hard_kill

**Invariant (added by EPIC #755 PR2, 2026-09-05):** every `cargo test`
invocation in the repo MUST export `MOAGAN_NON_INTERACTIVE=1`. PR #757
(EPIC #755 PR1) added the library-side env-var read at
`RunContext::default_interactive` (`src/phases/phase.rs:227`); this PR2
closes #736 by propagating the env var to every CI workflow job, every
Makefile target, `scripts/gauntlet.sh`, and every smoke script that
invokes `cargo test`. If a future change adds a new `cargo test`
invocation, the env-var prefix MUST be set on it — grep verification:

```bash
grep -rn 'cargo test' scripts/ .github/workflows/ Makefile docs/ \
  | grep -v 'MOAGAN_NON_INTERACTIVE' \
  | grep -v '^[^:]*:#' \
  | grep -v 'docs/test-skips.md'
```

Real invocations produce zero matches; the only remaining hits are
display labels (`name:`, `context:`), diagnostic `echo` strings, help
text (`@echo`), comments, and markdown table cells.

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

## Layer 8 — Process-wide env-locks (`TEST_*_LOCK` statics)

Tests that mutate process-wide state — `std::env::set_var`,
`std::env::set_current_dir`, etc. — must serialise against any
sibling test that reads the same state, or the parallel `cargo
test` run surfaces as a flake. Each lock here is a `pub static
Mutex<()>` declared at the top of `src/lib.rs` (under `#[cfg(test)]`
or, in the case of `TEST_API_KEYS_LOCK`, deliberately not gated so
integration tests in `tests/` can also acquire it). The lock costs
~8 bytes per process and is only ever touched by the test harness.

These are NOT skips — every test below still runs. They are
serialisation gates so the asserts inside each test see a stable
view of the env-var they touch.

### Inventory

| Lock | Env vars / state protected | Defined at | Acquired by (representative) |
|---|---|---|---|
| `TEST_MOAGAN_HOME_LOCK` | `MOAGAN_HOME` | `src/lib.rs:57` | `src/cli/{mod,repair,rate,doctor}.rs` tests, `src/cli/probe.rs`, `src/reconcile/mod.rs`, `src/config/{mod,profile}.rs`, `src/fs_layout.rs`, `src/phases/synthesize.rs`, `src/preferences/{cache,integration}.rs` (~50 tests) |
| `TEST_API_KEYS_LOCK` | `MINIMAX_API_KEY`, `DEEPSEEK_API_KEY`, `OPENCODE_API_KEY` | `src/lib.rs:70` | `src/llm/{api_keys,provider}.rs` tests, `src/cli/{doctor,probe}.rs` tests (~14 tests) |
| `TEST_PATH_LOCK` | `PATH` | `src/lib.rs:81` | `src/research/pdf.rs` tests (K.4 sub-1 PDF-parser block) |
| `TEST_MINIMAX_ENV_LOCK` | `MOAGAN_MINIMAX_MODEL`, `MOAGAN_MINIMAX_ENDPOINT` | `src/lib.rs:92` | `src/config/mod.rs` tests (PR-B2 config-precedence block) |
| `TEST_CWD_LOCK` | `std::env::set_current_dir` (cwd) | `src/lib.rs:99` | `src/config/mod.rs` tests (6 sites) |
| `TEST_EVENT_FORMAT_LOCK` | `MOAGAN_EVENT_FORMAT` (clap `env = "..."` binding at `src/cli/mod.rs:255`) | `src/lib.rs:133` | `src/cli/mod.rs::tests::event_format_default_is_auto`, `src/cli/mod.rs::tests::event_format_env_off_reaches_parser` (PR #678) |
| `TEST_LOG_TO_STDERR_LOCK` | `MOAGAN_LOG_TO_STDERR` (clap `env = "..."` binding at `src/cli/mod.rs:299`) | `src/lib.rs:166` | `src/cli/mod.rs::tests::log_to_stderr_env_accepts_shell_idiomatic_one`, `src/cli/mod.rs::tests::log_to_stderr_env_accepts_false` (PR #679) |
| `TEST_NON_INTERACTIVE_LOCK` | `MOAGAN_NON_INTERACTIVE` (env var read by `RunContext::default_interactive` at `src/phases/phase.rs:227` — closes #734 / EPIC #755 PR1) | `src/lib.rs:197` | `src/phases/phase.rs::tests::run_context_*` (9 regression tests added by PR1 of EPIC #755 — env-var precedence surface for `BoolishValueParser` accept/reject vocabulary, save-and-restore on every `RunContext::new` / `new_with_config` call) |

Plus one **module-local** lock that lives outside `src/lib.rs`
because its scope is bounded to a single module:

| Lock | Env vars / state protected | Defined at | Acquired by |
|---|---|---|---|
| `STDOUT_EVENTS_TEST_LOCK` (module-local `Mutex<()>` at `src/telemetry/stdout_events.rs:406`) | `MOAGAN_DECISION_FORMAT` | `src/telemetry/stdout_events.rs:406` | `src/telemetry/stdout_events.rs::tests::env_var_extends_opt_out` and adjacent (PR #677). Note: this lock is module-local, not crate-wide; a test that touches both `MOAGAN_DECISION_FORMAT` and any of the crate-wide locks must acquire both. |

### Established pattern (PR #246 → #677 → #678 → #679)

1. **One lock per env-var group**, declared once in `src/lib.rs`
   under `#[cfg(test)]`. The `TEST_API_KEYS_LOCK` precedent
   shows the gate can be dropped if integration tests in
   `tests/` also need to acquire it (the integration-test
   compilation unit doesn't see `cfg(test)` of the lib).
2. **Lock window matches the read window** — for `MOAGAN_HOME`
   and the LLM keys, the window is "from `set_var` through the
   dispatcher call that consumes the var"; for
   `MOAGAN_EVENT_FORMAT` / `MOAGAN_LOG_TO_STDERR` it is just
   "the `Cli::try_parse_from` parse window" because clap reads
   the env binding at parse time only.
3. **Save-and-restore the previous value** before mutating (PR
   #677 precedent at `src/telemetry/stdout_events.rs:557-567`,
   generalised by PR #678 and PR #679). The pattern is:

   ```rust
   let prev = std::env::var("MOAGAN_FOO").ok();
   let _guard = TEST_FOO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
   unsafe { std::env::set_var("MOAGAN_FOO", "bar"); }
   // ... test body ...
   drop(_guard);
   unsafe {
       match prev {
           Some(v) => std::env::set_var("MOAGAN_FOO", v),
           None => std::env::remove_var("MOAGAN_FOO"),
       }
   }
   ```

   The explicit `drop(_guard)` happens **before** the restore so
   a panic inside the test body still releases the lock; the
   save/restore pair guarantees the inherited value lands back
   on the env if the test panics between `set_var` and the
   restore block.
4. **Helper wrappers in test modules** (`fn lock_home_env(tmp:
   &Path)` in `src/cli/{mod,repair}.rs` and
   `src/reconcile/mod.rs`) bind the lock + env mutation into a
   single function so callers don't have to repeat the
   acquire / mutate / release dance. The helper name should
   name the env var (PR #679 item 3: `lock_home_env` not
   `lock_env`).
5. **Local alias over the crate-wide static** for tests that
   acquire the same lock from many call sites (see
   `src/cli/probe.rs:753` and `src/llm/provider.rs:2125` for
   the `TEST_API_KEYS_LOCK` alias pattern).

### Anti-pattern (do NOT do)

- **Do NOT use a lock for one env var to serialise mutations of
  a different env var.** That is the bug PR #678 closed for
  `MOAGAN_EVENT_FORMAT` (was riding on `TEST_MOAGAN_HOME_LOCK`)
  and the bug PR #679 closed for `MOAGAN_LOG_TO_STDERR` (was
  riding on `TEST_MOAGAN_HOME_LOCK`). The two vars are
  independent clap bindings — sharing a lock over-serialises
  and forces unrelated tests to wait on each other.
- **Do NOT skip the save/restore** "because the test always
  sets the var". A panic between `set_var` and the explicit
  cleanup leaks the mutated var to the next parallel test,
  re-creating the very flake the lock was added to prevent.
- **Do NOT add a new lock without a doc comment** matching the
  shape of the existing ones (`src/lib.rs:46-133`). The
  comment must list the env vars the lock guards, the call
  sites that acquire it, and the historical PR that closed
  the flake the lock exists to prevent.

### How to add a new lock

1. **First**: confirm the flake is real by stress-running the
   affected test with `cargo test --lib --no-fail-fast
   --test-threads=8 <name>` × 20. If a flake reproduces, the
   lock is justified; if it doesn't, the test has a different
   bug and the lock will only mask it.
2. **Declare** the static at `src/lib.rs` next to the existing
   `TEST_*_LOCK` siblings, with a full doc comment
   (env-var scope, call sites, historical PR).
3. **Acquire** in the affected test with the save/restore
   pattern above.
4. **Update Layer 8** here — add a row to the inventory table
   so the next maintainer doesn't have to grep the codebase
   to discover the lock.

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
| 8 | Process-wide env-locks (`TEST_*_LOCK` statics) | 7 crate-wide locks + 1 module-local | n/a (serialisation, not skip) |

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
   Add a clear `#[ignore = "reason"]` message. **For env-var or
   cwd mutations that must run in parallel** (i.e. cannot be
   `#[ignore]`d), follow the Layer 8 lock pattern instead — add a
   `TEST_*_LOCK` static to `src/lib.rs`, acquire it with
   save/restore, and update the Layer 8 inventory table.

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
