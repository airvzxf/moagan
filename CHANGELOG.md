# Changelog

All notable changes to `moagan` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.18] - 2026-08-29

### Changed

- **Close out the v0.10 `opencode_go` → `opencode` rename** —
  every remaining `opencode_go` / `OPENCODE_GO` /
  `OpenCodeGo…` / `opencode-go` identifier in `src/` is renamed
  to its canonical v0.10 form (`opencode` / `OPENCODE` /
  `OpenCode…` / `opencode:`). No schema or wire-format change;
  the upstream URL `https://opencode.ai/zen/go/v1/...` is
  untouched everywhere it appears. Surface area:

  - `src/llm/capabilities.rs` — `for_opencode_go()` →
    `for_opencode()`; `for_opencode_go_responses()` →
    `for_opencode_responses()`; `capabilities_for_opencode_go_responses_prefers_responses_wire`
    test renamed to
    `capabilities_for_opencode_responses_prefers_responses_wire`.
  - `src/llm/probe.rs` — `RE_OPENCODE_GO` static regex →
    `RE_OPENCODE`; the local `opencode_go` closure var →
    `opencode`; the `parse_cap_opencode_go_is_not_less_or_equal`
    test → `parse_cap_opencode_is_not_less_or_equal`.
  - `src/llm/openai_compat.rs` —
    `send_streaming_clamps_max_tokens_to_opencode_go_hard_cap`
    test → `send_streaming_clamps_max_tokens_to_opencode_hard_cap`;
    `ProviderCapabilities::for_opencode_go_responses()` call site
    → `for_opencode_responses()`.
  - `src/llm/openai_compatible.rs` — test fixture string
    `"opencode_go"` → `"opencode"` (provider-section name in the
    capability-matrix probe).
  - `src/llm/anthropic_compat.rs` — `OpenCodeGoMessagesResponseBody`
    → `OpenCodeMessagesResponseBody`;
    `OpenCodeGoMessagesContent` → `OpenCodeMessagesContent`;
    `OpenCodeGoMessagesUsage` → `OpenCodeMessagesUsage`.
  - `src/llm/{wire,wire_format,provider,deepseek}.rs` — comment
    references rewritten; no behavioural change.
  - `src/cli/doctor.rs` — `ProviderCapabilities::for_opencode_go()`
    and `for_opencode_go_responses()` call sites →
    `for_opencode()` / `for_opencode_responses()`.
  - `src/cli/probe.rs` — `--provider opencode-go:kimi-k3`
    doc-comment examples (max_tokens + temperature probes) →
    `--provider opencode:kimi-k3` (the canonical provider id is
    the section name `opencode`).
  - `tests/integration_discover_{deepseek,minimax}.rs` and
    `tests/integration_q3_dotenv.rs` — comment references
    rewritten.
  - `docs/test-skips.md` — Layer 6d skip block code sample
    rewritten.

  Historical `OPENCODE_GO_MAX_TOKENS_CAP` /
  `OpenCodeGoDispatch` / `OpenCodeGoProvider` / `BLOCKED_MODELS`
  breadcrumbs are preserved in the modified doc comments so the
  v0.9 / v0.10 lineage stays auditable; each breadcrumb
  references the v0.12.18 close-out explicitly.

- **Strip every remaining `opencode_go` / `OpenCodeGo…` /
  `OpenCode Go` / `opencode-go` reference from comments and
  docstrings** (follow-up to the close-out above). The rename
  left a layer of historical breadcrumbs in every `src/llm/*.rs`,
  `src/cli/doctor.rs`, the discover integration tests, and one
  workflow / doc file. Every current-behavior description that
  read "OpenCode Go" now reads "OpenCode"; every breadcrumb that
  referenced an obsolete identifier (e.g. `OpenCodeGoDispatch::url`,
  the legacy `BLOCKED_MODELS` gate, the v0.9 16 384-token cap,
  the v0.10 `opencode_go` → `opencode` section rename) is
  rephrased to preserve the historical context without naming
  the symbol that no longer exists. Files touched:

  - `src/llm/{anthropic_compat,openai_compat,openai_compatible,
    capabilities,probe,provider,json_strategy,embed/remote,
    response_format_opt_out,param_rejections,deepseek,
    temperature_probe,probe_table}.rs`
  - `src/cli/doctor.rs`
  - `tests/integration_discover_{deepseek,minimax,opencode}.rs`
  - `tests/integration_{probe_uses_min_output_tokens_for_thinking,
    probe_temperature,param_rejection_self_heal,q3_dotenv}.rs`
  - `docs/temperatures-auto.md`, `docs/test-skips.md`
  - `.github/workflows/test-ignored-opencode.yml`

  The upstream URL `https://opencode.ai/zen/go/v1/...` is
  untouched everywhere it appears; the cleanup is textual-only
  and no test semantics change.

- ci(workflows): capture stdout/stderr into separate 7 d `.log.gz` artifacts (`moagan-<job>-{stdout,stderr}`) with a generic failure hint naming the artifact pattern; existing `-logs` and `-jsonl-*` artifacts are unchanged.

## [0.12.17] - 2026-08-28

### Fixed

- **Test-only: CI guard against §2.2-style flakes via `tracing::debug!`
  in inline `mod tests`** ([#668](https://github.com/airvzxf/moagan/issues/668)).
  Mechanical check that flags any `tracing::debug!` / `tracing::trace!`
  macro inside a `#[cfg(test)] mod tests { … }` block in `src/`. The
  same `tracing_subscriber::fmt::try_init()` side-effect that PR #647
  worked around for the `--lib` binary would silently poison any
  future DEBUG/TRACE assertion added in a test module — this guard
  surfaces the mistake before it ships. New script
  `scripts/check-no-trace-debug-in-mod-tests.sh`; wired into
  `make guard-deps` (T0, < 2 s pre-commit + CI) so the check fires on
  every push. Three at-risk sites annotated in-code with a short
  docstring explaining the §2.2 mechanism and the migration path; no
  `src/` production-code API change; no schema bump.

## [0.12.16] - 2026-08-28

### Changed

- **`docs/temperatures-auto.md`** — the "How to disable the probe"
  section previously claimed *"There is no env-var toggle"* (line 91-94,
  pre-#657). Rewritten to document the new `MOAGAN_TEMPERATURE_AUTO`
  env var (off-spelling set `false`/`0`/`no`/`off`) and the matching
  per-provider `[providers.<name>] temperature_auto_enabled = false`
  TOML field as the operator-facing opt-out, with the hand-edit /
  delete-the-cache-file path kept as a last-resort fallback. The
  troubleshooting section's matching block was rewritten in the
  same shape.

- **`docs/cli-cheatsheet.md`** — the §0.2 env-var table now lists
  `MOAGAN_EVENT_FORMAT` and `MOAGAN_TEMPERATURE_AUTO` alongside
  `MOAGAN_LOG_FORMAT` / `MOAGAN_DECISION_FORMAT` /
  `MOAGAN_LOG_TO_STDERR`; the `MOAGAN_LOG_TO_STDERR` row notes the
  `BoolishValueParser` accepts `1`/`0`/`true`/`false`/`yes`/`no` /
  `on`/`off`. §0.3 ("What's new") is bumped from v0.12.14 to
  v0.12.15, and the default-value table grows an `--event-format`
  row recording the v0.12.15 default flip from `jsonl` to `auto`.

- **`docs/proposal-03-add-ons.md`** — §D.30.5 ("max-tokens
  auto-probe env overrides") grows a sibling paragraph for the
  temperature auto-probe documenting
  `MOAGAN_TEMPERATURE_AUTO` and the per-provider TOML field.

### Fixed

- **`src/cli/mod.rs`** — shrank the `EventFormatArg::Auto` and
  `event_format` field docstrings from 8 / 13 lines to 2 / 4 lines.
  The "issue #657 fix #N" historical paragraphs that grew out
  of the merge commits were moved to the commit message where
  they belong; the docstrings now read at the same density as
  the sibling `LogFormatArg::Auto` and `--log-format` field.

- **`tests/integration_e2e_script_paths.rs`** — dropped the
  pre-v0.12.15 workaround function `write_minimax_temperature_cache`
  (which hand-wrote a `temperatures_auto.toml` sidecar to dodge
  the 21-candidate temperature fan-out). Replaced with the new
  `MOAGAN_TEMPERATURE_AUTO=false` env var at the `moagan run`
  invocation site. The stale `FOLLOW-UP` note that predicted the
  fix and the stale `src/main.rs:57-66` overwrite-precedence
  comment were rewritten. The defence-in-depth
  `.env_remove("MOAGAN_EVENT_FORMAT")` was removed because the
  env var is now honoured end-to-end (issue #657 fix #2 closed
  the precedence gap; canonical proof lives in
  `tests/integration_stream_routing.rs::env_event_format_off_suppresses_stdout_events`).

**No public API change; no schema bump.** Docs + test-fixture
cleanup only — the runtime behaviour was already correct in
v0.12.15. Closes the follow-up flagged in the v0.12.15 release
body.

## [0.12.15] - 2026-08-28

### Fixed

- **`src/cli/mod.rs`** — `MOAGAN_LOG_TO_STDERR=1` was rejected by
  clap's strict `bool` parser (`error: invalid value '1' for
  '--log-to-stderr' [possible values: true, false]`), so an
  operator following the docstring hit a hard startup failure even
  though `resolve_log_to_stderr` already accepted the canonical
  `1`/`0`/`true`/`false`/`yes`/`no`/`on`/`off` spellings. The flag
  is now wired through `BoolishValueParser::new()` (same parser
  `--non-interactive` has used since v0.10). Issue #657 fix #1.

- **`src/cli/mod.rs` + `src/main.rs`** — an inherited
  `MOAGAN_EVENT_FORMAT=off` was silently discarded because
  `src/main.rs` overwrote the env var unconditionally from the
  CLI's `EventFormatArg::Jsonl` default arm. `EventFormatArg` now
  has an `Auto` variant (mirror of `LogFormatArg::Auto`), the
  default flips to `Auto`, and the `Auto => None` arm in the
  `event_override` match leaves the env var alone when the
  operator did not pass the explicit flag. The `env =` clap
  binding now also reads the variable at parse time. Issue #657
  fix #2.

- **`src/config/mod.rs` + `src/llm/provider.rs`** — the temperature
  auto-probe had no configurable opt-out; it fired for every
  non-mock `(provider, model)` and cost 21 requests in a fresh
  run. The pre-fix workaround (pre-populating
  `<MOAGAN_HOME>/temperatures_auto.toml`) is what #656's wiremock
  e2e test relied on. The new `MOAGAN_TEMPERATURE_AUTO` env var
  (plus `ProviderConfig::temperature_auto_enabled`) mirrors the
  `MOAGAN_MAX_TOKEN_AUTO` shape: `false`/`0`/`no`/`off` opts every
  provider out of the background fan-out; truthy values leave the
  default on; unrecognised values are silently ignored so a typo
  cannot silently disable the probe. The `TemperatureTable` still
  attaches to the registry so cached / operator-supplied entries
  continue to clamp runtime temperatures — only the background
  probe is suppressed. Issue #657 fix #3.

**No public API change; no schema bump.** Three pre-existing
operator-facing defects that the new wiremock e2e test had to
work around now work without the workaround.

## [0.12.14] - 2026-08-28

### Added

- **`tests/integration_e2e_script_paths.rs`** — new integration test that
  exercises the real-LLM CLI path against a `wiremock` upstream, closing
  the §2.4 gap where `make smoke` and `make e2e` (both `mock:mock-model`)
  could not detect the chain of four bugs fixed in v0.12.6–v0.12.10.
  It spawns `moagan audit proxy` against the mock and drives
  `moagan run --mode fast --provider minimax:MiniMax-M2.7` through it.
  Four tests, one per bug: the bare-provider rejection
  (`src/cli/mod.rs`), the wire-format suffix rejection
  (`src/llm/wire_format.rs`), the NDJSON `run_start` event plus the
  `max_tokens` operator cap (`src/llm/minimax.rs`), and the
  pattern-based proxy banner lookup (`src/cli/audit.rs`). Runs in ~9 s
  and joins T2; every assertion was verified discriminative by
  reintroducing the bug it pins.

### Fixed

- **`.github/workflows/e2e-network.yml`** — the retry loop always logged
  `rc=0`. `END_TS=$(date +%s)` reset `$?` before `RC=$?` read it, so the
  diagnostic reported `date`'s exit code instead of `make`'s, in both the
  `test-fast` and `test-explore` jobs. Every future e2e-network failure
  would have reported the wrong code. The loop also avoids
  `if make ...; then ... fi`, since an `if` compound with no `else`
  reports exit 0 when the condition is false.
- **`docs/e2e-loop-2026-08-12.md`** — the summary counts contradicted
  their own iteration lists (`3 fully green` listing four iterations,
  `7 with one flake` listing six). Corrected to 4 and 6.

### Documentation

- **`.github/workflows/e2e-network.yml`** — recorded the rationale for
  `MAX_ATTEMPTS=3` (§2.7), derived from the run distribution in
  `docs/e2e-loop-2026-08-12.md`, and corrected the `retry x4` labels to
  match the actual value. The comment separates what the run-log table
  records from what it only implies.
- **`AGENTS.md`** — rewrote the "Working with workflow" section around
  the invariant that validation precedes release because a tag is
  irreversible, and split the guidance by whether a change touches CI,
  since local T0–T2 run against `mock:mock-model` and cannot exercise a
  workflow edit.

**No source behavior change; no public API change; no schema bump.** No
files under `src/` were modified.

## [0.12.13] - 2026-08-28

### Fixed

Patch v0.12.13 (closes the §2.3 F2 follow-up cleanup) — drops 30+ stale
`opencode_go` / `opencode-go` / `OPENCODE_GO` references identified by
the F2 explorer subagents after the v0.12.12 §2.3 audit. **No source
behavior change; no public API change; no schema bump.** All edits
are cosmetic — comment updates, opaque test-fixture labels, and test
function renames where the body already uses the canonical v0.10
`opencode` name.

- **`src/cli/probe.rs`** — doc examples (`--provider opencode-go:kimi-k3`
  → `--provider opencode:kimi-k3`, ×3) and the `parse_provider_model`
  test now exercise the canonical v0.10 section name.
- **`src/cli/doctor.rs`** — test name `check_api_key_fails_when_opencode_go_key_missing`
  → `check_api_key_fails_when_opencode_key_missing`; comment
  `OPENCODE_GO` → `OPENCODE`.
- **`src/cli/telemetry_cmd.rs`** — test fixture `"opencode_go"` →
  `"opencode"`.
- **`src/config/mod.rs`** — comments (×2): `(Q7 opencode-go, etc.)`
  → `(Q7 opencode, etc.)`; `/v1/responses — 1 model` → `— 2 models`;
  test name `default_opencode_go_providers_*` → `default_opencode_providers_*`.
- **`src/llm/api_keys.rs`** — doc comment example `(e.g. "opencode_go")`
  → `(e.g. "opencode")`.
- **`src/llm/circuit_breaker.rs`** — test fixture strings.
- **`src/llm/deepseek.rs`** — doc comment `OpenCodeGoProvider` →
  `the opencode Anthropic-compatible provider`.
- **`src/llm/governor.rs`** — test fixture string.
- **`src/llm/http.rs`** — comment `(opencode_go_anthropic.rs)` removed.
- **`src/llm/openai_compatible.rs`** — test fixture strings (×3) +
  test names (×3) + comment.
- **`src/storage/sqlite.rs`** — test fixture strings + comments (×9).
- **`src/telemetry/dashboard.rs`** — test fixture strings + comment.
- **`src/telemetry/saturation.rs`** — test fixture string + comment.

### Verification

- `cargo build --release`: clean.
- `cargo test --all-targets`: 0 failures.
- `cargo clippy --all-targets -- -D warnings`: 0 warnings.
- `cargo fmt --all -- --check`: clean.

### Out of scope (deliberately kept)

Per the F2 'intentional' list, these remain on the v0.10 → v0.12.x
migration list and are NOT touched in this patch:

- `src/llm/capabilities.rs::{146, 167}` — `for_opencode_go()` /
  `for_opencode_go_responses()` constructors (back-compat).
- `src/llm/probe.rs::{486, 510, 512, 540, 1945}` — `RE_OPENCODE_GO`
  regex + `parse_cap_opencode_go_is_not_less_or_equal` test.
- `src/llm/openai_compat.rs::{1429-1583}` /
  `src/llm/anthropic_compat.rs::{892, 969}` / `src/llm/openai_compatible.rs`
  — v0.10 'legacy `OPENCODE_GO_MAX_TOKENS_CAP` is gone' breadcrumb
  comments.
- `src/phases/util.rs` `MiniMax-M3` references — canonical smoke-gate
  model + 'missing opening brace' pathology tests.
- `CHANGELOG.md` / `docs/*final-report.md` / `docs/pending-items-*` /
  `docs/discovery-validation-*` / `docs/proposal-*.md` — historical
  record of v0.6–v0.9 dispatching.

## [0.12.12] - 2026-08-28

### Fixed

Patch v0.12.12 (closes the §2.3 audit chain) — exercises the `card80` /
`discover_opencode` / `discover_deepseek` / `discover_opencode_models`
sections of `scripts/e2e_audit_proxy.sh` against the operator's
real API tokens (the v0.12.5 → v0.12.11 chain only validated `fast` +
`explore` against minimax). **No public API change; no schema bump.**
All edits are script / workflow / docs / config-test only — no
production-code paths in `src/` are touched (per §4 "out of scope").

- **`scripts/e2e_audit_proxy.sh`** — six latent bugs in the
  end-to-end smoke script that the v0.12.x chain never exercised:
  - The `OPENCODE_GO_API_KEY` env-var gate was a v0.9 stub the
    dispatcher no longer reads; the v0.10 resolver maps `opencode`
    → `OPENCODE_API_KEY` (`src/llm/api_keys.rs:40`). Renamed to
    `OPENCODE_API_KEY` in every guard; the `OPENCODE_GO_COVERAGE_MODELS`
    array renamed to `OPENCODE_COVERAGE_MODELS`.
  - The `--provider opencode_go` and `--provider deepseek` invocations
    in A.bis / A.ter rejected bare aliases with exit code 2
    (`build_registry_for: probe: expected 'provider:model', got 'X'`).
    Switched to `--provider opencode:mimo-v2.5` (A.bis), `--provider
    opencode:$MODEL` (A.quad), and `--provider deepseek:deepseek-v4-flash`
    (A.ter) — matches the v0.10 multi-model section shape.
  - The hardcoded `MiniMax-M3` grep in the card80 audit-log assertion
    (line 363-364) silently passed with 0 matches when the upstream
    was actually `MiniMax-M2.7` — replaced with `MiniMax-M2.7`.
  - The `MOAGAN_DISABLE_DEEPSEEK_NATIVE` gate was a pay-as-you-go
    budget guard; operator restored the native `DEEPSEEK_API_KEY`
    on 2026-08-28; gate removed.
  - The 14-model `OPENCODE_COVERAGE_MODELS` list trimmed to the
    operator's published 7-model roster (`deepseek-v4-flash`,
    `glm-5.3-flash`, `gpt-5.6-luna`, `mimo-v2.5`, `minimax-m2.7`,
    `muse-spark-1.2-contributor`, `qwen3.7-max`); all 7 are registered
    in `default_providers()` and cover all 3 wire formats.
  - The A.bis discover request now uses `mimo-v2.5` (the operator's
    smoke-test pin) instead of the undeclared `kimi-k2.7-code`.
- **`src/config/mod.rs`** — `default_providers()` register the new
  user-spec model IDs alongside the v0.12.x defaults:
  - `deepseek`: `deepseek-v4-flash`, `deepseek-v4-flash-vision-exp`,
    `deepseek-v4-pro` (the operator's 2026-08-28 roster); legacy
    `deepseek-chat` / `deepseek-reasoner` kept for back-compat with
    `tests/integration_discover_deepseek.rs:75` and the legacy
    operator fixtures.
  - `opencode`: `glm-5.3-flash`, `muse-spark-1.2-contributor`
    (the operator's two new aliases). All 17 v0.12.x aliases kept
    for back-compat with `tests/integration_capability_gating.rs`,
    `tests/integration_temperature_matrix_rewrite.rs`,
    `tests/integration_phase_k.rs`, and the opencode doc comments.
- **`.github/workflows/test-ignored-opencode.yml`** — renamed from
  `test-ignored-opencode-go.yml`; uses `OPENCODE_API_KEY` as the
  GitHub secret (the v0.10 dispatcher never read `OPENCODE_GO_API_KEY`).
  Companion cross-references in `ci.yml` and `test-ignored-minimax.yml`
  updated to the new path.
- **`.github/workflows/e2e-network-discover-{opencode,opencode-models,deepseek}.yml`**
  — three new `workflow_dispatch:`-only workflows (manual-only) that
  the Makefile targets and the audit script always referenced but
  the `.github/workflows/` directory lacked (PR #555 had deleted
  them alongside the auto-push triplet). Self-builds the release
  binary; ~10 min for opencode, ~20 min for deepseek, ~35 min for
  the 7-model opencode sweep.
- **`.github/workflows/e2e-network.yml`** — comment block references
  updated to the new workflow names.
- **`Makefile`** — `e2e-network-discover-opencode-go*` targets renamed
  to `e2e-network-discover-opencode*`; help text time budgets updated.
- **`tests/integration_discover_opencode.rs`** — renamed from
  `tests/integration_discover_opencode_go.rs`; the cargo test invocation
  in the workflow already targets the new name. The
  `discover_opencode_go_writes_four_subdirs` test function renamed to
  `discover_opencode_writes_four_subdirs`. The `OPENCODE_GO_MAX_TOKENS_CAP`
  reference in the `MOAGAN_MAX_TOKEN_AUTO` docstring removed (the
  constant was already gone in v0.10). Cross-references in
  `integration_discover_{deepseek,minimax}.rs` updated.
- **`tests/integration_discover_deepseek.rs`** — `--provider
  deepseek:deepseek-chat` → `--provider deepseek:deepseek-v4-flash`
  (the operator's 2026-08-28 canonical model name).
- **`docs/cli-cheatsheet.md`** — the operator-facing `moagan probe`
  examples now use `opencode:qwen3.7-max` and `opencode:kimi-k3`
  (the v0.10 section name); the `OPENCODE_GO_MAX_TOKENS_CAP` rows
  annotated with "(removed in v0.10; per-model ceiling replaced
  by the auto-probe persisted in `max_tokens_auto.toml`)" so the
  historical record still stands.
- **`docs/temperatures-auto.md`** / **`docs/max-tokens-auto.md`** —
  the TOML examples switched to `[providers.opencode.kimi-k3]` /
  `[operator_caps.opencode]` (the v0.10 section name).
- **`docs/branch-protection.md`** / **`docs/validation-tiers.md`** /
  **`docs/test-skips.md`** — workflow filename + test name references
  updated to the new names.

### Out of scope (deferred)

- **§2.4** (e2e + smoke `wiremock` integration test for the
  LLM-real path) — kept separate; will land in its own patch.
- **§2.7** (`MAX_ATTEMPTS=3` rationale comment in `e2e-network.yml`) —
  kept separate; 15-min doc-only patch.
- **§2.8** (`tracing::subscriber::with_default` isolation) — kept
  separate; the recent PR #647/#648 already mitigates the worst of
  this with the single-test-binary invariant; a deeper refactor is
  outside the scope of this audit.
- **src/ stale `opencode_go` references** — 30+ stale references
  remain in production code comments and test fixtures
  (`src/cli/probe.rs:778-779`, `src/cli/doctor.rs:487, 644`,
  `src/config/mod.rs:3343`, etc.). They are functionally inert
  (the dispatcher keys on the canonical `opencode` string), but
  should be cleaned up for consistency in a follow-up PR.

### Verification

- `make fmt-check guard-deps lint test build`: 0 failures.
- `MOAGAN_SMOKE_SECTION=discover_deepseek bash scripts/e2e_audit_proxy.sh`
  with the operator's `DEEPSEEK_API_KEY`: 4/7 assertions pass
  (`run_id_present`, `drafts_nonempty` soft-skip, `telemetry_plan_reports_weekly`
  soft-skip, `telemetry_plan_used_positive`). The 3 hard asserts
  (`tags_nonempty`, `facets_nonempty`, `extractions_subdirs`) still
  fail because the `deepseek-v4-flash` upstream emits trailing-comma
  JSON that `parse_json_with_recovery` cannot repair — a §9.2
  follow-up bug separate from this audit.

### Operator-side findings

- The `OPENCODE_API_KEY` in `~/.config/moagan/api_keys.toml`
  authorizes `/v1/models` (HTTP 200) but rejects every
  `/v1/chat/completions` request with HTTP 401. The script fix is
  correct; the upstream key needs a refresh before
  `discover_opencode` / `discover_opencode_models` can pass.

## [0.12.11] - 2026-08-28

### Fixed

Patch v0.12.11 (PRs #647 + #648) — fixes the §2.2 CI flake in `phases::util::tests::parse_json_with_recovery_preserves_extraction_metadata_via_tracing` (intermittent failures in PR #641, the release PR for v0.12.9). **No source-code changes; no public API change; no schema bump.** All edits are test-only or doc-only.

- **`src/phases/util.rs`** — removed `parse_json_with_recovery_preserves_extraction_metadata_via_tracing` from the `phases::util::tests` module (PR #647, –69 lines). The flake's root cause was structural: `src/sandbox/process.rs:2525`'s `moa_sandbox_run_cmd_with_off_logs_denial` calls `tracing_subscriber::fmt::try_init()`, which installs a process-global subscriber whose `EnvFilter` defaults to `LevelFilter::ERROR` when `RUST_LOG` is unset. That sets `LevelFilter::current()` (a process-global `AtomicUsize`) to `ERROR` for the whole test binary, silencing every `tracing::trace!` / `tracing::debug!` callsite on every thread. `tracing::subscriber::with_default` (thread-local) cannot reliably override this. The narrow race during `Dispatch::new → register_dispatch → rebuild_interest` was the actual flake surface. The fix moves the test to a fresh integration binary that gets its own `LevelFilter::current()` atomic and callsite registry — eliminating the contention entirely.
- **`tests/integration_parse_json_recovery.rs`** — new integration test binary (PR #647, +89 lines; PR #648, +20 / –2 lines). Reproduces the tolerant-extraction `tracing::debug!` event with structured `start, end` byte-range assertions, plus poison-recovery on the shared `Mutex<Vec<u8>>` buffer (a `PoisonError` still holds the bytes that were already written; the old `unwrap_or_default()` silently dropped them). Module doc carries an explicit "do not add additional `#[test]` functions to this binary" invariant to prevent reintroduction of the §2.2 flake. PR #648 also removes a no-op `.with_max_level(Level::TRACE)` writer wrapper (`WithMaxLevel::make_writer_for` always returns `Some` when the wrapper level is `TRACE`, the most permissive value) and the now-dead `MakeWriterExt` import.
- **`src/sandbox/process.rs:2514-2534`** — rewrite of the misleading docstring on `moa_sandbox_run_cmd_with_off_logs_denial` (PR #648, +14 / –4 lines). The original comment described `tracing_subscriber::fmt::try_init()` backwards: it claimed the only failure mode is "no subscriber installed", when `try_init` actually returns `Err` precisely when a global subscriber IS already installed. The new comment states the real side effect: `try_init` sets `LevelFilter::current()` to `LevelFilter::ERROR` for the entire test binary, and any new tracing-dependent unit test in this binary must live in its own integration test binary.
- **`src/phases/util.rs:469-481`** — breadcrumb added to the `parse_json_with_recovery` docstring (PR #648, +9 / –0 lines) pointing at `tests/integration_parse_json_recovery.rs` and explaining why the test must live in its own binary (cross-references the §2.2 flake and commit `1e3bb18`).

### Out of scope (deferred)

- M3 strategy `tracing::debug!` events (`src/phases/util.rs:495` and `:502`) have no tracing-test coverage. Adding them would conflict with the "single-test binary" invariant from PR #648; a separate `tests/integration_parse_json_recovery_m3.rs` binary is the right home for them and is tracked outside this patch.

### Verification

- `make test-ci`: 0 failures across 60 test binaries.
- `e2e-network` on main (run #33143518223): 4/4 jobs GREEN — Build · release binary, T3 · preflight — minimax, Tier 3 · e2e — fast, Tier 3 · e2e — explore.
- `e2e-network` retry history: run #33139298895 initially failed with HTTP 429 ("Token Plan rate limit reached") on the upstream MiniMax API, not on the new code; run #33139628433 (retry 1) and run #33143518223 (retry 2) both passed.

## [0.12.10] - 2026-08-27

### Fixed

Patch v0.12.10 (PR #642) — restorer for `e2e-network.yml` after v0.12.9 was published with a broken script fix. **No source changes, no public API change, no schema bump.**

- **`scripts/e2e_audit_proxy.sh`** — replace the broken `--max-tokens` CLI flag (v0.12.9 was based on a false assumption; `moagan run` doesn't expose that flag) with the correct mechanism: `MOAGAN_CONFIG=<path>` + a per-proxy TOML config that pins `[providers.minimax.models] max_tokens = 131072` for `MiniMax-M2.7` (per the models.dev catalog, operator-confirmed 2026-08-27). New helper `write_minimax_config` writes the TOML into each work dir. Also fixes a pre-existing flaky test: the wrapper was greping for the literal `run id` substring that only appears when stdout is a TTY (`src/cli/mod.rs:1580-1582`); in CI (piped stdout) the footer never appears, so the test failed even on successful runs. New pattern greps for `"kind":"run_(start|end)"` which is always present in the NDJSON event stream.
- **`.github/workflows/e2e-network.yml`** — `MAX_ATTEMPTS` set to 3 (operator-requested final value, absorbs occasional upstream flakes via the 60 s backoff tail).

## [0.12.9] - 2026-08-27

### Fixed

Patch v0.12.9 (PR #640) — fourth regression blocking `e2e-network.yml`, exposed after PR #638 (v0.12.8) restored the wire_format URL. **No source changes, no public API change, no schema bump.**

- **`scripts/e2e_audit_proxy.sh:289, 471, 499`** — added `--max-tokens 131072` to the 3 minimax invocations. `MiniMax-M2.7` rejects `max_tokens > 131072` per models.dev (operator-confirmed 2026-08-27); when the models.dev catalog is unreachable, the runtime falls back to `MINIMAX_MAX_TOKENS_CAP = 524_288` (src/llm/capabilities.rs:35) and the upstream rejects the call. The explicit flag forces the `effective_max_tokens` chain below the M2.7 ceiling. Per-model output cap overrides in `default_providers()` is the long-term fix and belongs to v0.13.0's config-schema redesign.

## [0.12.8] - 2026-08-27

### Fixed

Patch v0.12.8 (PR #638) — third regression blocking `e2e-network.yml`, exposed after PR #636 (v0.12.7) restored the `--provider minimax:model` invocation. **No source changes, no public API change, no schema bump.**

- **`scripts/e2e_audit_proxy.sh:289, 471, 499`** — `MOAGAN_MINIMAX_ENDPOINT=http://...:PORT/anthropic/v1` → `.../anthropic/v1/messages`. `src/llm/wire_format.rs:484` (introduced in PR #589, v0.10 schema refactor) rejects any URL without a recognised wire-format suffix (`/messages`, `/chat/completions`, `/responses`); the `/messages` suffix lets the client select the Anthropic-compatible wire format when sending through the proxy. The proxy's `--upstream` argument is unchanged (it forwards any path under that base). Default MiniMax model in the e2e harness is now `MiniMax-M2.7` (was `MiniMax-M3` in v0.12.7) per operator request; `MiniMax-M3` remains available as a section alias.

## [0.12.7] - 2026-08-27

### Fixed

Patch v0.12.7 (PR #636) — second of two regressions blocking `e2e-network.yml`, exposed after PR #634 (v0.12.6) restored the wrapper's ability to find the proxy's banner. **No source changes, no public API change, no schema bump.**

- **`scripts/e2e_audit_proxy.sh:289, 471, 499`** — `--provider minimax` (bare section) → `--provider minimax:MiniMax-M3`. The `minimax` section grew to 4 models (recently adding `MiniMax-M2.7-highspeed`), and `src/cli/mod.rs:1402` (introduced in PR #589, `refactor(config)!: v0.10 schema refactor`) rejects bare-section providers when the section has >1 model with exit code 2. The canonical first model (`MiniMax-M3`) matches what `docs/e2e-loop-2026-08-12.md` used when fast mode was last green. The sibling `opencode_go` and `deepseek` sections are single-model aliases, so their bare-provider invocations (lines 566, 681) keep working.

## [0.12.6] - 2026-08-27

### Fixed

Patch v0.12.6 (PR #634) — restores the `e2e-network.yml` workflow after a 4-day 100% red window (last green: run `32619993732`, 2026-08-23 05:17 UTC). **No source changes, no public API change, no schema bump.**

- **`scripts/e2e_audit_proxy.sh:210` + `scripts/smoke_audit_proxy.sh:102`** — `head -1 "$portfile"` → `grep -m1 'proxy listening' "$portfile"`. The audit-proxy wrapper captures the proxy's combined stdout+stderr with `> portfile 2>&1` and was reading the first line, expecting the `proxy listening on http://...` banner. Commit `b1193ac` (2026-08-25 21:22 UTC, PR #614 `feat(observability): saturate tracing coverage`) added a `tracing::debug!("init_tracing: subscriber initialised")` at `src/main.rs:343` that fires before the banner — its JSON line became the first line of the portfile and the `head -1` started returning JSON instead of the banner, breaking the `*proxy*listening*` match. The pattern-based search is robust to any pre-banner tracing noise (current or future). The downstream port extraction (`grep -oE 'http://127.0.0.1:[0-9]+'`) is unaffected because the matched line still contains the URL.

### Retracted

- **`docs/ci/e2e-network-findings.md`** (shipped in `368b4f2` / PR-04c, retracted in `3f59540` / PR #634). The doc misdiagnosed the regression as the v0.12.0 stream-routing flip. The actual culprit was `b1193ac` (a tracing event added 5 days after v0.12.0). The corrected root cause fits naturally in the fix commit's message body; no standalone findings document was warranted for a 1-line wrapper fix.

## [0.12.5] - 2026-08-27

### Changed

Patch v0.12.5 (PR-04c) — CI hygiene + e2e debug. **No source changes, no public API change, no schema bump.**

- **C-2 (CI JSONL artifacts)**: `.github/workflows/ci.yml` now uploads moagan JSONL sidecars (`**/*.jsonl.gz`, `**/*.jsonl`) as GitHub Actions artifacts with **retention 7 days** on the 5 code-running jobs (`test-tests`, `test-lib`, `test-doc`, `smoke`, `e2e`). The 3 non-code jobs (`fmt-check`, `guard-deps`, `clippy`) are intentionally untouched. `MOAGAN_HOME` is pinned to `$GITHUB_WORKSPACE/.moagan-home` per-job so the `actions/upload-artifact` glob resolves under the workspace (the default `$HOME/.local/share/moagan` would be outside the artifact scope). `actions/upload-artifact` is SHA-pinned to `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1`, matching the existing pin in `e2e-network.yml:133` and `cargo-audit.yml:62` (consistency across all 4 upload sites). `if-no-files-found: warn` keeps a green job green when no sidecars were produced. The operator can now download the JSONL stream of any failed CI run within 7 days, instead of having to reproduce locally.

- **C-3 (e2e-network investigation)**: timeboxed 2 h budget; the investigation finished in ~6 minutes with **no actionable CI fix identified**. 38 of 38 in-scope failures (Aug 20–27 UTC, post-restructure) are categorised as `moagan-bug`, not CI infra (`upstream-flake` = 0%, `ci-timeout` = 0%, `infra-flake` = 0%). Two regression patterns are documented: (a) `moagan-bug-proxy-start` (12 runs) — `moagan audit proxy` hangs after the `dispatching` log and never prints `proxy listening` within the script's 10 s timeout; (b) `moagan-bug-audit-log` (26 runs) — proxy starts but `moagan run` returns rc=2 or rc=7 and the wrapper fails the audit-log assertion. Last green run was `32619993732` (2026-08-23 05:17 UTC). The hypothesis is that the v0.12.0 stream-routing flip (`feat!: route tracing logs to stdout by default [v0.12.0]`) changed how the proxy's startup message reaches the wrapper, but the root cause requires triage in `src/cli/audit/proxy.rs` — **explicitly out of scope for PR-04c, deferred to v0.13.0+**. Full categorised run table + next-step triage checklist: `docs/ci/e2e-network-findings.md`.

## [0.12.4] - 2026-08-27

### Changed

Patch v0.12.4 (PR-04b-2) — precision and observability hygiene. No breaking changes. Sidecars from v0.12.3 remain readable; the next probe-run / ranking-persist overwrites the file with the cleaner format.

- **A-4 + N-6 (`serde_clean_f32`)**: `f32` values that cross the runtime boundary (TOML sidecars at `<MOAGAN_HOME>/temperatures_auto.toml`, JSON sidecars in `<run_dir>/`) are now serialised via Ryu's shortest round-trip decimal (`0.1`, not `0.10000000149011612`). New module `src/serde_util/clean_f32.rs` with three variants — `vec` for `[f32]` / `Vec<f32>` (`Entry::temperatures`, `OperatorCap::temperatures`); `scalar` for single `f32` (`JudgeScore::score`, `RankEntry::score`, `SketchTags::similarity_to_category`, `Cluster::cohesion`, `JudgeScoreEntry::score`, `AdversaryReport::disagreement_score`, `AdversaryReport::score_delta`); `opt_scalar` for `Option<f32>` (`Ranking::stability_sigma`). The helper deserialises via the default `f32` deserialiser so v0.12.3 sidecars (`"score": 0.85000002384…`) and the clean form (`"score": 0.85`) both land on the same `f32` bits. No `schema_version` bump.

  **Why this is needed**: `Rust 1.55+` already formats `f32` via Ryu (so `format!("{0.1_f32}")` is `"0.1"`), but `toml_edit::ValueSerializer::serialize_f32` — the encoder the runtime's TOML sidecars go through — widens every `f32` to `f64` via `serialize_f64(v as f64)`. The `as f64` cast preserves the `f32` bit pattern verbatim (so `0.1_f32` becomes `0.10000000149011612_f64`), and TOML emits the latter as a 17-digit decimal. The helper routes the value through `format!("{f32}").parse::<f64>()` so the widened `f64` is the shortest-round-trip decimal, not the lossy widening. `serde_json` already uses Ryu on `f32` directly so the helper is mostly a no-op for JSON sidecars, but applying it explicitly is cheap, future-proofs against a backend that widens `f32 → f64` before emitting, and keeps TOML / JSON wire shapes uniform.
- **N-1 (`RewriteEvent` cardinality signal)**: `ExplorationMatrix::RewriteEvent` now carries `original_count`, `unique_count`, `dropped_count`, `effective_fanout_per_cell` alongside the existing `n_clamped`. The dispatcher's audit log gets an additional `tracing::info!("discovery: temperature profile collapsed after upstream clamping")` whenever `dropped_count > 0` — a profile declared as `[0.1, 0.12, 0.14, 0.5, 0.52, 0.9, 0.91]` with an upstream that only accepts `[0.1, 0.5, 0.9]` now reports `original_count=7 unique_count=3 dropped_count=4` instead of silently looking like a 7-temperature profile. Operators can `grep dropped_count > 0` to find every collapse.
- **N-2 (`1e-3_f32` band-dead threshold)**: the gates at `src/phases/phase.rs:1168` and `src/discovery/matrix.rs:486` that compare `clamped` vs `requested` now use `1e-3_f32` (was `f32::EPSILON ≈ 1.19e-7`). The wider band catches the Ryu-vs-TOML widening gap (`0.7` → TOML `0.70000004768` — same bits, different decimal forms after `as f64`) and 1-decimal operator rounding (`0.3` → TOML `0.30000001192`), without swallowing meaningful changes (`0.5 → 1.0` is 0.5 away, well above the threshold).
- **N-5 (spans use `Display::fmt`)**: four `tracing` span fields that were silently widening `f32` to `f64` now use `%` for `Display::fmt` — `temperature_profile = %temperature` at `src/discovery/coordinator.rs:881, 916, 1038` and `candidate = %temperature` at `src/llm/temperature_probe.rs:338`. JSONL span context emits `"candidate": 0.7` instead of `"candidate": 0.7000000476837158`.
- **N-4 (docs verified, no edit needed)**: `docs/temperatures-auto.md:138-169` already documents the clean Ryu format. No doc edit; a new pin test (`persisted_sidecar_uses_ryu_shortest_round_trip`) prevents a future refactor from regressing the on-disk shape.

### Tests

Nine new unit tests (2281 → 2290 passing):

- `src/serde_util/clean_f32.rs::tests::serde_clean_f32_emits_shortest_round_trip_decimal` — pins that `0.1`, `0.3`, `1.7` round-trip clean and the TOML widening blobs (`0.10000000149`, `0.30000001192`, `1.70000004768`) never appear in serialised output.
- `src/serde_util/clean_f32.rs::tests::serde_clean_f32_preserves_operator_precision_up_to_ryu_limit` — bit-identity via `to_bits()` for `[0.1, 0.75, 1.123, 1.1234, 1.7]`.
- `src/serde_util/clean_f32.rs::tests::serde_clean_f32_does_not_touch_strings_outside_temperatures` — pin that the helper does NOT touch `String` fields on a mixed-field struct (operator-side timestamps and labels stay verbatim while `f32` neighbours pass through Ryu).
- `src/serde_util/clean_f32.rs::tests::serde_clean_f32_handles_nan_and_infinity` — pins the NaN / ±inf contract end-to-end: the helper itself never panics on NaN/inf (Ryu emits `NaN` / `inf` / `-inf`), but a `scalar`-annotated field whose value is NaN serialises to JSON `null` and the subsequent deserialisation fails with `invalid type: null, expected f32`. This is the load-bearing behaviour against silent data corruption — operators who encounter NaN in production must treat the sidecar as poisoned and re-probe from scratch.
- `src/serde_util/clean_f32.rs::tests::serde_clean_f32_scalar_variant` — `RankEntry { score: 0.85_f32 }` → JSON `"score": 0.85` and back to bit-identical `0.85_f32`.
- `src/serde_util/clean_f32.rs::tests::serde_clean_f32_opt_scalar_variant` — `Option<f32>` with `Some(0.85)` and `None`.
- `src/discovery/matrix.rs::tests::rewrite_event_exposes_collapse_signals` — fixture N-1: declared `[0.1, 0.12, 0.14, 0.5, 0.52, 0.9, 0.91]` vs upstream `[0.1, 0.5, 0.9]` → `original_count=7`, `unique_count=3`, `dropped_count=4`, `effective_fanout_per_cell=6`.
- `src/discovery/matrix.rs::tests::rewrite_clamps_near_equal_but_not_bit_identical` — N-2 threshold pin: `0.10000005_f32` (distance ~5e-8) does NOT trigger a `RewriteEvent` (below `1e-3_f32`); `0.71_f32` (distance 0.01) DOES trigger one (above `1e-3_f32`). A regression that restores `f32::EPSILON` fails the first case.
- `src/llm/temperature_probe.rs::tests::persisted_sidecar_uses_ryu_shortest_round_trip` — N-4 on-disk pin: the persisted `temperatures_auto.toml` contains `0.1`/`0.5`/`0.9` and does NOT contain `0.10000000149`/`0.50000005960`/`0.89999997616`.

## [0.12.3] - 2026-08-27

### Fixed

Patch v0.12.3 over v0.12.1. The version skips v0.12.2: a v0.12.2 release was originally cut from the PR branch before squash-merge into `main`, leaving the tag pointing at a commit that is not reachable from the canonical history. The tag and GitHub Release for v0.12.2 were retracted; the production code that v0.12.2 would have shipped is identical to what v0.12.3 ships.

**Production fixes** (the actual A-1/A-2/A-3/C-1 scope of PR-04b-1):

- **A-1**: race `MINIMAX_API_KEY` / `MOAGAN_MAX_TOKEN_AUTO` `set_var` / `remove_var` in parallel tests. The pre-existing `ENV_LOCK` pattern from `tests/integration_mvp.rs:36` was applied across the suite that mutates API-key env vars — `tests/integration_auto_probe_persists_files.rs` (3 tests) AND `src/llm/provider.rs::tests` (11 tests). Two pre-existing flakes flagged by the operator on closing the original bug — `registry_auto_probe_persists_both_toml_files` and `registry_table_floor_takes_the_highest_opted_in_provider` — both pass under `--test-threads=4` 5/5.
- **A-2**: banner "moagan continue --kind discovery …" is now TTY-gated in `run_resume` (`src/cli/discover.rs:1276`). The sibling gate at line 886 was already added in PR-04a; this PR extends the same pattern to the resume entry point so any `moagan continue` invocation piped into `jq` produces a clean NDJSON stream.
- **A-3**: `build_provider_for_probe` in `src/cli/probe.rs` now propagates the section name (e.g. `"minimax"`) to `ResolvedModelConfig::section` instead of the model id. The pre-fix bug wrote `section: model_id.to_owned()`, which caused `MinimaxProvider::from_resolved` (and the deepseek wrapper) to look up the API key under the uppercased model id (e.g. `MINIMAX-M3_API_KEY`) and miss — operators running `moagan probe minimax MiniMax-M3` saw `InvalidApiKey` errors.
- **C-1**: clap `env = "MOAGAN_NON_INTERACTIVE"` binding on every `non_interactive: bool` subcommand field (`Cmd::Run`, `Cmd::Continue`, `Cmd::Resume`, `Cmd::Discover`, `Cmd::Preflight`), with `clap::builder::BoolishValueParser::new()` so the Makefile's `MOAGAN_NON_INTERACTIVE=1` is parsed correctly (clap's default bool parser only accepts `true`/`false`). The `Makefile` `test` and `test-ci` targets now export `MOAGAN_NON_INTERACTIVE=1` so `cargo test --all-targets` never blocks on a stdin prompt. CLI > env > default precedence is automatic via clap's built-in `env =` feature (matches the existing `MOAGAN_LOG_FORMAT` / `MOAGAN_RUNS_DIR` / `MOAGAN_LOG_TO_STDERR` / `MOAGAN_DECISION_FORMAT` pattern in the same file).

**Test-only follow-ups** (no production behavior change; addresses review feedback on PR #628):

- **A-2 test uselessness**: the original `discover_banner_suppressed_*` integration test ran `moagan discover --help`, which exits via clap parse before any banner code runs — the assertion was vacuously true. Refactored the two banner prints (`src/cli/discover.rs:886` discover / `src/cli/discover.rs:1276` resume) into helper functions `write_discover_banner<W: Write>` and `write_resume_banner<W: Write>`. Deleted `tests/integration_probe_section.rs`. Added 3 unit tests in `src/cli/discover.rs::tests` that capture the banner through `Vec<u8>` and pin the `is_terminal()` gate via `include_str!` source check.
- **`TEST_API_KEYS_LOCK` unification**: the initial patch added three file-local `static ENV_LOCK` instances (in `src/llm/provider.rs::tests`, `src/cli/probe.rs::tests`, and `tests/integration_auto_probe_persists_files.rs`). Replaced them with `crate::TEST_API_KEYS_LOCK` (the existing crate-wide mutex at `src/lib.rs`), so a parallel `src/cli/doctor.rs::tests` or `src/llm/api_keys.rs::tests` test can no longer race the same `MINIMAX_API_KEY` mutation through a sibling lock. Removed the `#[cfg(test)]` gate on `TEST_API_KEYS_LOCK` itself so integration tests can reach it via `moagan::TEST_API_KEYS_LOCK`; the lock is zero-sized and only the test harness touches it.
- **Doc comment accuracy**: replaced the contradictory paragraph at `src/llm/provider.rs:2064` (claimed "The lock is held only for the set/remove pair (microseconds)" while the implementation actually holds the lock across the `await` call) with one that matches the real behaviour and explains why `await_holding_lock` is necessary.

### Tests

- `tests/integration_auto_probe_persists_files.rs::env_lock_serializes_minimax_api_key_mutations` (A-1, 8 OS threads contending for the lock).
- `tests/integration_auto_probe_persists_files.rs::probe_propagates_section_name_not_model_id` (A-3, on-disk TOML header with `model id ≠ section name`).
- `src/llm/provider.rs::tests::env_lock_serializes_minimax_api_key_mutations_in_provider_tests` (A-1, API-contract pin).
- `src/cli/probe.rs::tests::build_provider_for_probe_uses_section_not_model_id` (A-3, direct construction-seam test).
- `src/cli/discover.rs::tests::write_discover_banner_emits_expected_shape` (A-2 follow-up, banner content).
- `src/cli/discover.rs::tests::write_resume_banner_emits_expected_shape` (A-2 follow-up, banner content).
- `src/cli/discover.rs::tests::discover_banner_is_gated_by_is_terminal` (A-2 follow-up, gate pin via source check).

## [0.12.1] - 2026-08-27

### Fixed

- **Test-only: stop two `/tmp/moagan-*` tempdir leak classes** ([#623](https://github.com/airvzxf/moagan/pull/623), [#626](https://github.com/airvzxf/moagan/pull/626)). Two distinct leak bugs in the test suite were closed in the same week. Neither changes production code, the public API, or operator-visible behavior.

  - **PR #623** stopped the historical `std::env::temp_dir().join("moagan-…")` pattern in tests that left ~19 GB of cruft on tmpfs between runs (audit documented in `docs/discovery-validation-research-2026-08-13.md`). Migrated to `tempfile::TempDir` whose `Drop` impl cleans up on the panic path; added the CI guard `scripts/check-no-tempdir-leaks.sh`.
  - **PR #626** stopped `src/discovery/coordinator::tests` (15 of 22 `#[test]` functions) leaking `/tmp/moagan-discovery-coordinator-*` dirs because `new_coordinator_with_mode` returned a `DiscoveryCoordinator` (which owns an r2d2 SQLite pool) out of `with_moagan_home`'s closure. The `TempDir::drop` raced the pool's open FDs and `remove_dir_all` returned `ENOTEMPTY`, silently swallowed by `tempfile::TempDir::drop`. Closed by adding a sibling helper `with_moagan_home_keep<F, R>(label, f) -> (TempDir, R)` that returns the tempdir to the caller so its drop runs **after** the coordinator closes its FDs. The module now leaves **0** leaked dirs per `cargo test --lib discovery::coordinator::tests` run (was 15).

  No migration required. All 22 tests in `discovery::coordinator::tests` preserved (none `#[ignore]`d, no helper removed).

## [0.12.0] - 2026-08-26

### Changed (BREAKING)

- **Stream routing flip (PR-04a / E-1): `moagan` now sends tracing logs to `stdout` by default; only `ERROR`-level events still go to `stderr`.** This unblocks the canonical Unix split:

  ```text
  moagan run …  1> out.jsonl  2> errors.jsonl
  jq -c 'select(.kind=="llm_call")' out.jsonl   # domain NDJSON events
  grep '"level":"ERROR"' errors.jsonl          # tracing ERRORs only
  ```

  The internal `init_tracing` registers a single `fmt::layer()` whose writer (`RoutingWriter`) uses `make_writer_for` to dispatch each event to stdout or stderr based on its `Metadata` level. The decision lives in the writer (per-event, per-thread, not per-layer-filter), which avoids a thread-local-coherence regression that the v1 draft (two layers + per-layer `filter_fn`) hit on the multi-threaded tokio runtime: `tokio::spawn` workers emitted events that bypassed the per-layer filter pipeline, leaking ~14 non-ERROR events to stderr on a real discover smoke. The writer-side decision is also simpler to reason about — one writer, one routing table — and lines up with the secret-redaction `ReportingLayer` wrapper from `src/telemetry/redact.rs` that already rides on top.

  Migration timeline:
  - **v0.12.0** (this release): routing flip is on by default.
  - **v0.13.0**: `--log-to-stderr` warning is reinforced; no behavioural change.
  - **v0.14.0**: flag removed; scripts that still need the legacy routing should switch to `1> out.jsonl 2> errors.jsonl`.

- **Discover banner is now TTY-gated (A-2).** The human-readable `moagan discover <id> provider=… -> <path>` line printed by `src/cli/discover.rs:875` used to print unconditionally, breaking NDJSON purity for any operator piping stdout into `jq`. The print is now wrapped in `if std::io::stdout().is_terminal() { println!(…) }`; the non-TTY path emits the equivalent `tracing::info!` event so consumers see a structured line in the stdout stream.

### Added

- **`--log-to-stderr` global flag (deprecated).** Honours `MOAGAN_LOG_TO_STDERR=1` env var too. Restores the v0.11 "all-logs-on-stderr" behaviour for scripts that still pipe `2> log.jsonl`. A `DEPRECATED` warning is emitted via the tracing subscriber (so the operator sees the v0.14.0 removal deadline). Removed in v0.14.0.

### Changed

- **Redundant `eprintln!("error: …")` fallbacks removed.** `src/lib.rs:248-250` and `src/cli/mod.rs:1130-1137` previously emitted a duplicate plain-text `error: …` line gated on `stderr.is_terminal()`. With E-1, the structured `tracing::error!` is always routed to stderr (and only to stderr), so the duplicate is gone. TTY users still see a readable `error: …` line because `fmt::layer().text()` renders the JSON `error` field on the rendering side.
- **`eprintln!` warnings migrated to `tracing::warn!` for consistent routing.** Eleven call sites in `src/cli/discover.rs` (3 sites), `src/cli/run.rs` (2), `src/cli/continue_cmd.rs` (5), and `src/phases/rank.rs` (1) used to write plain-text warnings to stderr on the failure path. They now flow through the tracing subscriber, so they honour the routing flip (`stdout`), `--log-format`, and `RUST_LOG` filtering.

### Tests

- New `tests/integration_stream_routing.rs` pins the routing invariants:
  - `moagan --help` and `moagan doctor` write to stdout only (stderr empty).
  - Clap parse errors go to stderr only (stdout empty).
  - `--log-to-stderr` swaps the level→stream mapping (TRACE/DEBUG/INFO/WARN → stderr; ERROR → stdout) and emits the `DEPRECATED` warning to stderr.
  - The `moagan discover` banner is suppressed when stdout is not a TTY.
  - A clean `--event-format off` mock fast run leaves stdout free of `\"ERROR\"` literals and stderr empty.

## [0.11.2] - 2026-08-26

### Fixed

- **CRITICAL BUG: pipeline span's `run_id` was null for 697/707 events during dispatch.** PR #619's `cli::dispatch` returning `DispatchResult` was a structural improvement, but the `pipeline_span` was constructed with `tracing::field::Empty` placeholders and patched via `Span::record()` AFTER dispatch returned. Events emitted during dispatch inherited the empty span, so 98.9% of the JSONL had `pipeline.run_id = null`. Operators could grep `pipeline{run_id=...}` correctly only for RunStart/RunEnd.
  Fix: `run_with_cli` now pre-allocates the `RunId` (parses CLI input for Resume/Rerun/Refine/Rerank/Import; generates fresh UUIDv7 for Run/Discover/Preflight/read-only) BEFORE constructing the span. The `pipeline_span` is now built with `run_id = %run_id` from the start. Cascading changes: `dispatch(cli)` → `dispatch_with_run_id(cli, run_id)`; `run::run(opts, cfg, run_id)` and `discover::run(opts, cfg, run_id)` accept the pre-allocated id; the existing `dispatch(cli)` wrapper allocates a fresh id for compatibility with the existing test callers.
- **`Event::RunEnd.status` no longer hard-codes `"ok"` regardless of exit code.** Now `if exit_code == 0 { "ok" } else { "error" }`, matching the documented contract and enabling downstream `| jq 'select(.kind == "run_end" and .status == "error")'` filters.
- **`docs/events-v1.md` description of `run_start` timing** is now consistent with the implementation (was a stale pre-PR #619 claim).

## [0.11.1] - 2026-08-26

### Added

- **Real `run_id` in `Event::RunStart` / `Event::RunEnd`.** The CLI dispatcher (`src/cli/mod.rs::dispatch`) now returns `DispatchResult { exit_code, run_id, mode, provider, model, prompt_hash }` so the stdout event bus carries the actual RunId (e.g. `01a03d8c-5452-7333-96db-037022b62b3b`) instead of the `"pre-dispatch"` placeholder. Read-only commands (Inspect, Doctor, Validate, …) emit `"<read-only>"` as a stable sentinel. The `pipeline` span's `run_id` field is recorded after dispatch via `Span::record`, replacing the placeholder. **Behavior change**: `Event::RunStart` is now emitted AFTER the pipeline phase events (not before) — this is required to stamp the real run_id; downstream consumers should treat `RunStart` / `RunEnd` as a completion pair.
- **`Event::Decision` emits at 9 curated decision points**, gated by the new `--decision-format <off|summary|all>` flag (default `summary`; env `MOAGAN_DECISION_FORMAT`). The 9 sites: `winner_picked`, `low_confidence_winner`, `cluster_skipped`, `repair_applied`, `portfolio_finalized` (Summary by default); `category_assigned`, `judge_verdict`, `cache_hit`, `cache_miss` (only with `--decision-format=all` — high-volume sites opt in, not opt out). Helper in `src/telemetry/stdout_events.rs::emit_decision` keeps call sites readable.
- **`Event::DiscoveryIteration` events from the sketch loop** in `src/discovery/coordinator.rs`. Emitted per attempt with three outcomes: `"accepted"` (thesis ≥ 30 chars), `"rejected"` (thesis too short), `"error"` (extraction failed after retries). Each event carries `n`, `total`, `cell_dim`, `cell_facet`, `temperature`, `replica`, `sketch_index`, `outcome`.
- **`Event::Probe` parity for `probe_kind=temperature`.** `src/llm/temperature_probe.rs::probe_send_temperature` now emits the same `Event::Probe` shape that `src/llm/probe.rs` (max_tokens) already does, including a per-transport `Arc<AtomicU32>` counter that labels each call with its sequential iteration index within a fan-out. The `TemperatureProbeTransport` trait gains a default-impl `fn iteration_counter(&self) -> Option<Arc<AtomicU32>> { None }` (zero-cost; existing mocks untouched).

### Fixed

- **`[moagan] loaded .env from …` no longer pollutes stderr NDJSON.** The message was a pre-existing `eprintln!` in `src/main.rs:11-18` that ran before `init_tracing()` and broke the `moagan … 2>log.jsonl | jq` contract. Now emitted via `tracing::info!(target: "moagan::boot", dotenv_path = %path, "main: .env loaded (auto-discovered)")` AFTER `init_tracing()`, so it honours `--log-format` (one NDJSON line or coloured text), `RUST_LOG` filtering (e.g. `RUST_LOG=info,moagan::boot=off`), and the legacy `MOAGAN_QUIET=1` opt-out. Closes the follow-up noted on PR #618.

## [0.11.0] - 2026-08-26

### Removed (BREAKING)

- **`--logs <PATH>` flag and `MOAGAN_RUN_LOGS` env var are removed.**
  v0.10 shipped an opt-in file log writer as a non-standard
  workaround. POSIX shells already provide the right primitive
  (redirection), and the indirection only made the JSON output
  harder to wire into `jq` / Promtail / Loki. The replacement is:
  - stderr (FD 2): trace events, JSONL by default
    (`moagan 2> run.jsonl | jq .` parses cleanly).
  - stdout (FD 1): typed domain events in JSONL
    (`moagan > events.jsonl` for `phase_start`, `llm_call`, `probe`,
    `run_start`, `run_end`, …).
  See `docs/events-v1.md` for the event schema.

### Added

- **POSIX-idiomatic stderr routing (per ADR-0002 §B).** `init_tracing`
  now writes `fmt::layer().json()` to stderr by default when stderr
  is not a TTY, and `fmt::layer()` with file/line/target colouring
  when stderr IS a TTY. The auto-detection is override-able via
  the new `--log-format <text|json|auto>` global flag and the
  `MOAGAN_LOG_FORMAT` env var. Closes the gap that ADR-0002 §B
  originally specified (the v0.10 implementation wrote plain text
  to stderr).
- **Stdout events JSONL.** A new `src/telemetry/stdout_events.rs`
  module with a typed `Event` enum (RunStart, RunEnd, PhaseStart,
  PhaseEnd, PhaseError, LlmCall, Probe, Warning, Decision,
  DiscoveryIteration). Each event is one NDJSON line on stdout
  when stdout is not a TTY, silenced in interactive mode (operators
  get a clean terminal). Override with `--event-format
  <jsonl|off>` or `MOAGAN_EVENT_FORMAT`. Pipe-friendly:
  `moagan … 2>log.jsonl | jq -c 'select(.kind=="llm_call")'`.
- **Tracing span hierarchy** (Android-2018 ambient-context model).
  `pipeline` → `phase` → `iteration` → `llm_call` /
  `llm_probe` / `llm_probe_background`. Every event emitted by
  an LLM call (regular OR probe) now carries the full chain:
  `pipeline{run_id, mode, provider, model, resumed} + phase{name,
  seq} + iteration{n, total, cell, temperature, replica,
  sketch_index} + llm_call{call_id, role}`. Operators can grep
  one phase or one iteration to follow the entire timeline
  end-to-end. The redact hot path stays event-free.

### Fixed

- **Plain-text errors no longer break NDJSON consumers.** The
  dispatcher's `eprintln!("error: …")` is now conditional on
  `std::io::stderr().is_terminal()` so the operator gets a
  one-liner on a TTY but the JSONL stream stays clean for
  `moagan … 2>log.jsonl | jq`.
- **`llm_call` span propagated to events emitted across `.await`.**
  The previous implementation used `let _enter = call_span.enter()`
  at `src/phases/phase.rs:1274`, which only entered the span on
  the calling thread — the future inside `provider.send()` resumes
  on a different worker across every `.await`, dropping the span
  context (and on every rejection-cascade retry). The smoke
  histogram showed zero `llm_call` events: `pipeline=1587,
  phase=1012, llm_call=0`. Fix: wrap the dispatch sequence in
  `async { … }.instrument(call_span)` (see
  `src/phases/phase.rs:1585`), so `Instrument` re-enters the span
  on every poll. Histogram after the fix: `pipeline=676,
  phase=460, llm_call=260`, with `Event::LlmCall` and every
  `cache.store.*` / `telemetry.call.*` / `cost.record.*` event
  inheriting the `llm_call{call_id, provider, model, role}` slot.
- **Stdout NDJSON purity under non-TTY.** Three pre-existing
  human-affordance prints leaked to stdout regardless of whether
  it was a TTY, corrupting `moagan … | jq` consumers:
  - start banner at `src/cli/run.rs:328-336`,
  - `run id:` footer at `src/cli/mod.rs:1314-1316`,
  - checkpoint prompts at `src/checkpoint/human.rs:361-364`.
  Each is now gated on `std::io::stdout().is_terminal()`, so the
  TTY UX is preserved while pipe consumers see pure NDJSON. Smoke
  verdict: `exit=0, stdout NDJSON ✅`.
- **Removed false `--event-format auto` doc.** The doc comment at
  `src/cli/mod.rs:210-216` claimed `auto` is a valid alias for
  `jsonl`, but `EventFormatArg` only has `Jsonl | Off` and clap
  correctly rejects `auto`. The doc now matches the enum — `auto`
  was removed from the prose and `off` is documented as the only
  non-default value.

## [0.10.0] - 2026-08-24

### Breaking

- **`config.toml` schema v0.10 — per-model endpoints, no more singletons**.
  The v0.9 `[providers.<name>]` shape with `kind` / `endpoint` /
  `model` / `max_tokens` / `hard_incompatibilities` singletons is gone.
  The canonical schema is:

  ```toml
  [providers.minimax]
  endpoint = "https://api.minimax.io/anthropic/v1/messages"
  models = [
    { id = "MiniMax-M3",         max_tokens = 1000000 },
    { id = "MiniMax-M2.7",       max_tokens = 1000000 },
    { id = "MiniMax-M2.7-highspeed", max_tokens = 1000000 },
    { id = "MiniMax-M2.5",       max_tokens = 1000000 },
  ]

  [providers.opencode]
  models = [
    { id = "kimi-k3",   endpoint = "https://opencode.ai/zen/go/v1/chat/completions", max_tokens = 1000000 },
    { id = "minimax-m3", endpoint = "https://opencode.ai/zen/go/v1/messages",        max_tokens = 1000000 },
    { id = "gpt-5.6-luna", endpoint = "https://opencode.ai/zen/go/v1/responses",    max_tokens = 1000000 },
    # …other OpenCode models…
  ]

  [providers.deepseek]
  endpoint = "https://api.deepseek.com/v1/chat/completions"
  models = [
    { id = "deepseek-chat",    max_tokens = 1000000 },
    { id = "deepseek-reasoner", max_tokens = 1000000 },
  ]
  ```

  * `endpoint` is now `Option<String>` (rename of the intermediate
    `endpoint_new`). Per-model `endpoint` overrides the section
    default; this is how a single `[providers.opencode]` section
    groups the chat-completions, Anthropic, and Responses paths.
  * `models[].endpoint` overrides the section-level `endpoint`
    when set, so a single canonical section can host every
    wire-format variant the upstream exposes.
  * `kind` / `model` / `max_tokens` / `hard_incompatibilities`
    fields on the section are gone — they only powered legacy
    code paths that are no longer in the tree.

- **`**OPENCODE_GO_MAX_TOKENS_CAP** global constant removed**. The wire
  layer no longer applies a 16 384 ceiling to every OpenCode Go
  call. The auto-probe (`max_token_auto`) discovers the real
  upstream boundary per `(provider, model)` and persists it to
  `<MOAGAN_HOME>/max_tokens_auto.toml`. With no probe result and
  no operator override, the wire body carries the request's
  `max_tokens` unchanged.

- **`minimax` routed through its own provider** (not through
  `AnthropicCompatProvider`). Before v0.10, every `minimax` call
  was clamped to `OPENCODE_GO_MAX_TOKENS_CAP = 16_384` even though
  MiniMax's real ceiling is `MINIMAX_MAX_TOKENS_CAP = 524_288`.
  v0.10 dispatches `[providers.minimax]` to `MinimaxProvider`,
  which clamps at the wire body to the real MiniMax boundary.

- **Per-model alias sections collapsed into canonical
  families**. `[providers.kimi-k3]`, `[providers.minimax-m3]`,
  etc. no longer exist as separate sections. The operator
  reaches every model via `--provider SECTION:MODEL`
  (`--provider opencode:kimi-k3`).

- **`--provider PROVIDER[:MODEL]` is mandatory** on every
  LLM-touching command (`run`, `discover`, `preflight`).
  Resolution order (highest first):
  1. CLI flag.
  2. `MOAGAN_DEFAULT_PROVIDER` env var.
  3. `[defaults] provider` in `~/.config/moagan/config.toml`.

  When none is set the command exits with:

  ```
  --provider is required (or set MOAGAN_DEFAULT_PROVIDER, or
  [defaults] provider in config.toml); example:
    moagan run --provider opencode:kimi-k3 --prompt "..."
    moagan run --provider opencode --model kimi-k3 --prompt "..."
  ```

- **`--model <name>` removed**. The model id is the second
  half of `--provider SECTION:MODEL`. The CLI still parses
  `--model` to surface a friendly error so legacy scripts do
  not silently break.

- **`api_keys.toml` / `<NAME>_API_KEY` env-var names**: the
  canonical provider-family key is the **section name** —
  no more `kind` indirection. `OPENCODE_API_KEY` (was
  `OPENCODE_GO_API_KEY`), `MINIMAX_API_KEY`, `DEEPSEEK_API_KEY`
  stay unchanged; legacy `OPENCODE_GO_API_KEY` exports are
  no longer consulted.

### Changed

- **`ProviderCapabilities::wire_format_id()` returns `"anthropic"`
  / `"openai"` / `"openai_compatible"`** to match the serde
  renames on `WireFormatId`. Log lines, JSON serialisation, and
  the in-memory capability matrix all agree.
- **`Config::default_provider`** is `Option<String>` (was
  `String`). The empty-string check stays as the canonical
  "no default" sentinel so v0.9 `default_provider = ""` keeps
  parsing cleanly.
- **`Config::resolve_provider(raw) -> Result<(section, model)>`**
  is the new helper every CLI dispatcher uses to parse the
  `--provider` value. Validates the section exists, that the
  requested model id is registered under it, and that
  multi-model sections reject the bare `SECTION` form.

### Removed

- `OPENCODE_GO_MAX_TOKENS_CAP` global constant.
- `ProviderConfig::endpoint_str()` / `model_str()` accessors.
- `ProviderConfig::kind`, `::model`, `::hard_incompatibilities`,
  and `::max_tokens: Option<u32>` fields (v0.9 singletons).
- `BLOCKED_MODELS` list in `opencode_go.rs` (deprecated; the
  operator now decides which models route through OpenCode via
  their `config.toml`).

## [0.9.12] - 2026-08-23

### Fixed

- **TOML quoting inconsistente en `temperatures_auto.toml` y `max_tokens_auto.toml`**: el crate `toml` no envuelve keys en comillas cuando son bare (e.g. `kimi-k3`). Nuevo helper `quote_provider_model_keys` post-procesa el output de `toml::to_string_pretty` para que las keys `[providers."X"."Y"]` se serialicen siempre entre comillas, consistente con `config.toml`. (PR #587)

- **Visibilidad del clamp de temperatura en logs**: el coordinator del discovery mode ahora emite `temperature_profile` (no `temperature`) en el log de iteración, dejando claro que es el valor del profile post-rewrite, no el valor enviado. `RunContext::dispatch_to_provider` añade un `tracing::trace!` (`RUST_LOG=moagan::phases::phase=trace`) cuando el gate encuentra que la temperatura ya está en el set soportado. (PR #587)

- **Regex `credit_card` matcheaba f32 precision**: la regex original `\b(?:\d[ -]?){13,16}\b` matcheaba los 15 dígitos de `1.899999976158142` (f32 precision de `1.9`), redactando el `9` en logs como `temperature=1.[REDACTED:credit_card]`. La nueva regex requiere separadores visibles (` ` o `-`) entre grupos de 4 dígitos y descarta secuencias largas sin separadores. Trade-off documentado en el doc-comment del patrón. (PR #587)

## [0.9.11] - 2026-08-23

### Added

- **Auto-detection of supported sampling temperatures per `(provider, model)`**
  (PR #585). New module `src/llm::temperature_probe` that mirrors the
  `max_tokens` auto-probe pattern: discover the discrete set of temperatures each
  upstream accepts and persist it so the runtime can rewrite user-requested
  values without re-running the pipeline. The canonical candidate set is
  `[0.0, 0.1, ..., 2.0]` (21 values, `0.1` step); the probe fans out in groups
  of 3 so it never saturates the upstream, and persists the result in
  `<MOAGAN_HOME>/temperatures_auto.toml`. The runtime table
  (`TemperatureTable`) exposes `nearest_supported(...)`, which
  `RunContext::dispatch_to_provider` queries: when the temperature resolved by
  the per-role default / profile / matrix-profile falls outside the
  discovered set, it is clamped to the nearest neighbour and a
  `tracing::warn!` is emitted. The operator-supplied matrix profile is
  rewritten to the boundary in `coordinator.rs` (nearest-neighbour per
  `provider_model`), preserving the declared cardinality.

  CLI: new subcommand
  `moagan probe temperature --provider PROVIDER:MODEL [--persist-union]
  [--batch-size N] [--dry-run]`. `--persist-union` takes the union (not
  intersection) of the discovered sets across every model of the same
  provider and writes the resulting set into the sidecar as the operator
  cap (`auto = false`). The default `--batch-size = 3` matches the runtime
  constant `TEMPERATURE_PROBE_BATCH_SIZE` so the CLI never exceeds the
  auto-probe's own concurrency envelope at startup. Tests: 23 unit
  (`temperature_probe.rs`) + 5 CLI (`cli/probe.rs`) + 9 integration
  (`tests/integration_*_temperature_*.rs`). New docs:
  [`docs/temperatures-auto.md`](docs/temperatures-auto.md) (algorithm,
  sidecar format, troubleshooting).

### Changed

- **`moagan probe` now lists `max-tokens` and `temperature`** as
  verb-first subcommands. The dispatch lives in
  `src/cli/probe.rs::ProbeCmd`. Cheatsheet: §20.

## [0.9.6] - 2026-08-21

### Fixed

- **Per-provider circuit breaker cascade on facet-deriver 429**
  (the bug surfaced in `run-real-600` with v0.9.5). Two-tier
  recovery: `Error::Throttled` (transient HTTP 429 with RPM/TPM
  message) is absorbed by the per-`(provider, role)` adaptive
  throttle governor (AIMD backpressure that drops concurrency and
  increases the pre-call backoff with jitter); `Error::PlanExhausted`
  (token plan / monthly quota / subscription keywords) trips the
  per-`(provider, role)` circuit breaker. The legacy per-provider
  breaker is kept only for the provider pool's `is_available()`
  signal; it no longer short-circuits `send()`.

- **Missing-open-brace repair pass** (PR #575): the MiniMax-M3
  upstream occasionally emits the object body without the outer
  `{` (e.g. literally `"problem": "Design a calculator", ...}`).
  The tolerant extractor could not find a `{` or `[` to balance, so
  the chain returned `Error::SchemaViolation(rc=7)` and the whole
  run aborted on the very first sketch. New `RepairKind::OpenBrace`
  pass prepends `{` when the (possibly BOM- or prose-prefixed)
  input starts with a JSON key. Runs explicitly before Path B
  extraction so Path B sees the balanced object instead of the
  inner `["..."]` substring.

- **Self-inflicted circuit-open cascade guard** (PR #575): on
  `card-1576 par-512` the upstream returned `PlanExhausted` (429)
  for individual sketches. The per-`(provider, role)` breaker
  tripped, but every subsequent call on the already-open path
  returned the synthetic `PlanExhausted("circuit open: ...")` and
  the post-call match caught that same variant and re-armed the
  breaker via `record_failure()`. The breaker never recovered.
  Now `was_open` is captured pre-call and `record_failure` only
  fires on `PlanExhausted` when the breaker was CLOSED at the
  start of the call. Self-inflicted circuit-open errors are no-ops.

### Added

- **`Error::Throttled { retry_after_ms, message }`** variant —
  HTTP 429 with a non-plan body now classifies as transient
  rate-limit (handled by the throttle governor) rather than the
  pre-v0.9.6 `PlanExhausted` blanket-classification.
- **`Error::provider_cause()`** — helper that categorises
  provider-side errors into `PlanExhausted`, `Throttled`, or `Other`
  for the call-site governor / breaker to consume.
- **`ThrottleGovernor` + `GovernorRegistry`** (in
  `src/llm/governor.rs`) — per-`(provider, role)` AIMD
  backpressure. Reduces per-role in-flight concurrency and
  increases the pre-call backoff on transient 429s, recovers
  via additive increase + decay. Configurable via
  `[throttle_per_role]` in `~/.config/moagan/config.toml` or
  `MOAGAN_THROTTLE_PER_ROLE_<role>=<initial>:<max>:<init_backoff>:<max_backoff>:<additive_after>:<jitter>`.
- **`BreakerRegistry`** (in `src/llm/circuit_breaker.rs`) —
  per-`(provider, role)` breaker registry. Replaces the v0.9.4
  per-provider breaker. Configurable via
  `[circuit_breaker_per_role]` in `~/.config/moagan/config.toml` or
  `MOAGAN_CIRCUIT_BREAKER_PER_ROLE_<role>=<threshold>:<window_secs>:<cooldown_secs>`.

## [0.9.5] - 2026-08-21

### Fixed

- **`.meta.json` sidecar leak in discovery walks** — `discover_cluster`,
  `discover_facet`, and `discover_tag` filtered on
  `extension == "json"` only, which also matched the
  `.meta.json` sealed sidecars the FS layer writes next to every
  artefact. Because every domain struct (`Sketch`, `Cluster`,
  `FacetList`, `SketchTags`) is `#[serde(default)]`, the sidecars
  deserialised cleanly into records with empty ids / cluster_ids /
  empty facet lists. The downstream extract phase then saw 579
  phantom clusters with `members:[""]` and skipped them, yielding
  zero facet extractions and aborting with
  `discover_extract produced zero facet extractions`. Reproduced
  pre-fix on `run-real-600` (`01a0228d-da36-7583-afc0-52bd3b825d82`):
  1156 sketch files (578 real + 578 sidecar), 1160 cluster files
  (579 with `members:[""]` + 1 cluster_00 with all 578 real
  members). The filter that already excluded `.meta.json` in
  `discover_extract`, `discover_summary`, `discover_integrate`,
  `discover_contradict`, `propose`, and `discovery::context` is
  now factored into `phases::util::primary_json_paths` and used by
  the three missing call sites. End-to-end verified post-fix on
  card-80 par-8 (mini-validate / `01a022d9-…`): 80 sketches, 2
  clusters (0 phantom), 1 facet list (0 empty `cluster_id`), 6
  facet `.md` extractions, rc=0 elapsed=292 s. Sister fix:
  setting `MOAGAN_RATE_LIMIT_ROLE_FACET_DERIVER=30:2` (alongside
  the v0.9.4 tagger knob) is required when running with
  `--sketches-per-cell 10` (4-dim × 2-facet matrix → 80
  sketches) or higher — the facet-deriver fan-out similarly
  rate-limits upstream and defaults to an empty facet list when
  the upstream returns 429s (verified on the same mini run
  without the env var: 6 extractions vs 0).

## [0.9.4] - 2026-08-21

### Added

- **Per-role rate-limit (catalog §D.19.6)** — new `[rate_limit_per_role]`
  TOML knob (also reachable via env `MOAGAN_RATE_LIMIT_ROLE_<role>`
  or `RateLimitConfig::default()` extension) throttles a single role
  independently of the per-provider bucket. Resolves the run06
  cascade where the post-matrix tagger fan-out (1500+ LLM calls)
  saturated the upstream provider's quota and produced
  1484+ placeholder clusters with empty `label`/`summary`, which
  then inflated `discovery_context.json.facet_ids` to 15,668.
  Empty by default (no per-role limit) so existing runs are
  bit-identical. Operators opt in with, e.g.:
  ```toml
  [rate_limit_per_role]
  tagger = { capacity = 30, refill_per_sec = 2 }
  ```
  Acquired in `call_with_retry` after the cache lookup and in
  `call_uncached` so the retry path (which bypasses the cache)
  honours the same per-role bucket. CLI wiring at
  `src/cli/discover.rs` parses each string key into a `Role` via
  `<str>::parse::<Role>()`; unknown role names are silently
  skipped so a stale config never aborts the run.

## [0.9.2] - 2026-08-20

### Fixed

- **Parser chain stability** (#562) — `repair_missing_separators` in
  `src/phases/util.rs` now infers object context when the stack is empty
  and the next non-whitespace char is `:`. The previous behaviour re-inserted
  a stray `,` between a JSON key and its colon, breaking the lenient
  repair chain for fragments without a leading `{`. Unit-tested in
  `phases::util::tests::parse_model_json_does_not_reintroduce_comma_after_stray_comma_fix`.
  Closes `#558`.

- **SanCov profraw rotation** (#563) — the `*.profraw` file emitted by
  the
  SanCov runtime coverage instrumentation previously grew unbounded
  (~12 GB/hour on a healthily running pipeline). Added
  `CoverageRecorder::start_rotation(max_bytes, interval_secs)` that
  rotates the active file when it exceeds `MOAGAN_COVERAGE_PROFRAW_BYTES_MAX`
  (default 1 GiB). The slice capture is renamed to a timestamped
  sibling; merge with `llvm-profdata merge *.profraw.*` for cumulative
  coverage.

- **SanCov runtime warning** (#562) — `moagan` now emits a
  `tracing::warn!` at startup when the binary was built with the
  `coverage` Cargo feature AND `LLVM_PROFILE_FILE` is set, so the
  operator knows the runtime file will grow unbounded without
  rotation. The `start_rotation` thread spawned by `Telemetry::open`
  keeps the warning actionable.

- **Run-comparison script hardening** (#562) — `scripts/comparison/run-comparison.sh`
  now traps SIGINT/SIGTERM and quarantines failed runs to
  `.failed-<unix-ts>` siblings instead of leaving 60+ GB on disk
  when the underlying discovery run is aborted. Verified context:
  run8 on 2026-08-19 left a 66 GB `active.profraw` because the
  previous cleanup was conditional on `rc=0`.

- **Sketch extraction retries** (#564) — sketches that fail JSON parse
  on the first attempt are now retried up to 2 times (3 attempts
  total). Run8 on 2026-08-19 had a 4.2 % rejection rate; the
  additional retry recovers the majority of JSON-fragment failures
  caused by the temperature 1.0+ pathology on MiniMax-M3, without
  re-introducing the 30-day cardinality 880 projection that motivated
  the original drop from 3 to 1.

- **Stale `clippy` comment** (#564) — the audit comment in
  `src/cli/discover.rs:359` documenting the `--max-parallelism`
  cap was last touched when the cap was 64. PR #543 lifted it to
  `u32::MAX` (1) and PR #544 scaled the rate limiter with the cap;
  the comment is now refreshed to match the current state without
  implying a 64-call hard limit that no longer exists.

### Changed

- **Rate limiter refill** (#563) — the rate limiter default
  `refill_per_sec` was previously `parallelism / 4`, calibrated for
  the old sequential discovery loop where the bottleneck was a
  single concurrent call. The discovery loop is now actually
  parallel (see `perf` below), so the semaphore and the rate
  limiter are the same knob (both limit in-flight calls). The
  default now matches 1:1: `refill_per_sec = parallelism`. Operators
  who want a lower rate than the parallelism cap can override with
  `MOAGAN_RATE_LIMIT_<provider>`.

### Performance

- **Discovery loop parallelisation** (#563) — `src/discovery/coordinator.rs`
  spawned each `(cell, temperature, replica, sketch_index)` iteration
  as a separate `tokio::task` via a `tokio::task::JoinSet`
  (per AGENTS.md: no `tokio::spawn` without a recorded join handle).
  The semaphore (`ctx.parallelism`) limits the number of concurrent
  LLM calls. The previous implementation was sequential because
  the loop awaited each call in-place; the semaphore was acquired
  and released immediately, and only one permit was ever in flight.
  Throughput was ~3.3 sketches/min on every run regardless of
  `--max-parallelism` (verified against run7 8h 1619 sketches and
  run8 5h 40m 1348 sketches). The parallel loop honours the
  configured parallelism; a `--max-parallelism 64` run that took
  8 hours now targets the same workload in ~15-20 min.

- **Discovery lock discipline** (#563) — `SketchLoopState` and
  `SaturationTracker` are now wrapped in `Arc<std::sync::Mutex<>>`
  so concurrent tasks can safely mutate them. The mutex is held
  only for the mutation + `state.save`; the lock window is a few
  microseconds, dominated by the LLM round-trip (15-18 s).

### Added

- **`moagan preflight` subcommand** (#564, #566) — smoke-tests the
  full pipeline end-to-end against the real provider. Two-run flow:
  1. `moagan discover` with cardinalidad 8 (1 sketch per dimension
     × facets_per_dimension = 1), 1 temperature (1.0), 1 replica.
  2. `moagan run --mode fast` with `--context <discover_run_id>
     --context-full` so the second run consumes the discover run's
     library through the cross-run `--context` plumbing.
  Both run ids are printed on stdout so the operator can drill
  into either one with `moagan inspect`. Cost: ~60-120 s of API
  budget and ~3 MB of disk per preflight. Exits 0 only when both
  runs succeed; a non-zero exit code is a strong signal that the
  pipeline broke somewhere.

- **`scripts/smoke_preflight.sh`** (#566) — T3 smoke coverage for
  the preflight subcommand. Four tests against the mock provider
  in `tests/fixtures/mock_provider`:
  `preflight_two_run_ids_printed`, `preflight_mock_creates_discover_sketches`,
  `preflight_non_interactive_no_prompts`, `preflight_invalid_provider_fails_fast`.
  Wired into the `Makefile` `SMOKE_SCRIPTS` list so `make smoke` (the
  T3 gate in the validation gauntlet) runs it.

### Internal

- The `src/discovery/coordinator.rs` loop now uses
  `tokio::task::JoinSet` and `let _permit = ctx.parallelism.acquire()`
  inside each spawned task. Stop conditions (target reached, tracker
  `Stop`, cancel tripped) are polled between spawns; in-flight tasks
  continue to whatever they have in flight before the `JoinSet`
  drains.

- `startup_reconcile` is invoked at the top of every pipeline-opening
  dispatch (T3 D.28.3 + D.28.4). It reconciles filesystem vs SQLite
  and sweeps orphan `*.tmp.<uuid>` and stale `*.lock` files. The
  `Config::startup_reconcile` flag (default `true`) and
  `MOAGAN_STARTUP_RECONCILE=false` env var gate the call.

## [0.9.1] - 2026-08-19

### Fixed

- Anthropic prefill `{` for JSON-required roles (#554) — the wire
  layer now emits a leading `{` in the assistant prefill when the
  response payload is expected to be a JSON object. Avoids the
  upstream MiniMax model stalling on the first character of the
  response.

[0.9.2]: https://github.com/airvzxf/moagan/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/airvzxf/moagan/compare/v0.9.0...v0.9.1
