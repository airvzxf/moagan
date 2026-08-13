# Cross-session coordination protocol

> **Established** Wed Aug 12 2026 06:03 UTC, by the **models-dev session**.
> This document is shared by all autonomous sessions working on
> `airvzxf/moagan` concurrently.

## Sessions active today

| Session | Working on | Worktrees | Branch prefix | Status |
|---|---|---|---|---|
| **models-dev** | `models.dev/api.json` catalog integration (10 PRs) | tbd | `feat/models-dev-*`, `fix/probe-*`, `docs/models-dev` | active, ~6h budget |
| **e2e-network loop** | trigger `e2e-network.yml` continuously + triage failures | `.worktrees/fix-<scope>` (only when fixing) | `fix/audit-*`, `fix/e2e-*` | active, **window 06:24 UTC → 12:00 UTC 2026-08-12** |

Each session writes to this file **before pushing to main** so the other
session knows what landed.

## Communication channels

- **`docs/COORDINATION.md`** — protocol, current state, blockers
- **`docs/e2e-loop-2026-08-12.md`** — e2e-network flake log (watcher writes here)

## Branching rules (all sessions)

1. **All branches from `origin/main`**. No direct commits to `main`.
2. **Conventional commits** (`feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `ci`, `build`, `perf`).
3. **GPG-signed** (`414687A3CD7E65B9`). No `--no-gpg-sign`.
4. **No amend, no force** on shared branches.
5. **Before push**: `git pull --ff-only origin main` to catch up.
6. **Worktrees**: `~/.config/opencode/worktrees/moagan/<branch>` (or `.worktrees/<branch>`).
7. **Sequential PRs within one session** can be merged locally before pushing.

## Smoke / e2e gate

Every PR runs:
- T0: `make fmt-check` + `make guard-deps`
- T1: `make lint` + `make build`
- T2: `make test-ci`
- T3: CI (`make smoke` + `make e2e`); `e2e-network` is manual for `main`

If a PR breaks `e2e-network`, the **e2e-network loop session** opens an
issue with the run URL, the test that flaked, and the suspected PR. The
**active session** (whichever is in flight at that moment) investigates,
opens a fix PR, and links it in `docs/e2e-loop-2026-08-12.md`.

### Fix-takeover deadline (e2e-network loop only)

The e2e-network loop session runs continuously. If a `e2e-network` failure
whose root cause is a commit landed by another session is **not fixed**
within **3 consecutive loop iterations (~60 min wall-clock)** from the
moment the issue is filed, the e2e-network loop session **takes over**:
it creates a `.worktrees/fix-<scope>` from main, branches `fix/<scope>`,
implements the fix, validates, and squash-merges via the standard 10-step
PR protocol. The e2e-network loop session then resumes the loop.

This rule exists because the e2e-network loop blocks the rest of the
window while a regression is open; a 60-min cap keeps the loop moving
without unfairly pre-empting the other session's good-faith attempts.

## What NOT to do

- Don't rebase a branch the other session may depend on. Use merge or
  fast-forward only.
- Don't `git push --force`. Ever.
- Don't merge `main` into your branch — rebase if you must, but prefer
  merge for shared branches.

## Current state (live)

| Time (UTC) | Event | By |
|---|---|---|
| 06:03 | Coordination protocol created | models-dev |
| 06:24 | e2e-network loop started, window 06:24 → 12:00 UTC | e2e-network loop |
| 07:05 | PR #413 (coordination protocol) merged | models-dev |
| 07:30 | PR #416 (PR-0 probe fixes) merged; PR #417 (PR-1 catalog), PR #418 (PR-8 docs) created | models-dev |
| 08:50 | PRs #420-#424 created (PR-2 cap, PR-3 reason, PR-4 modal, PR-5 cost, audit-1) | models-dev |
| 09:00 | Audit #1 (inconsistencies-audit-2026-08-12.md, 580 LoC) | models-dev |
| 09:25 | PR #423 smoke fix; user_version assertions advanced to v015 | models-dev |
| 09:30 | PRs #425 (PR-7 CLI), #426 (audit-2) created; PR #427 (audit-3) created | models-dev |
| 10:10 | PRs #420, #422, #423, #424, #425, #426, #427 merged into main | models-dev |
| 10:50 | Discovery: ModalityGate / cost_estimate / CapabilityResolver are not yet wired into the call site — planned for round 4 | models-dev |
| 10:56 | 10-PR plan + 3 audit rounds MERGED. main = 9379a67. 1869 lib tests pass. | models-dev |

## Final state of the 10-PR plan (merged at 10:56 UTC)

| # | PR | What it shipped |
|---|---|---|
| 0 | #416 | fix(probe): M1, M2, M3, M7, M8, M9, C2 follow-ups from PR #401 |
| 1 | #417 | feat(llm): models.dev static catalog fetch + 1h TTL cache |
| 2 | #420 | feat(llm): capability resolver + temperature gating via models.dev |
| 3 | #421 | feat(llm): reasoning gating via models.dev catalog |
| 4 | #422 | feat(llm): modality, attachment, tool_call gating via models.dev |
| 5 | #423 | feat(llm): cost_usd from models.dev catalog (SQLite v015) |
| 6 | #424 | refactor: round-1 audit fixes (drop lease_full, flags_batch, wire token_budget) |
| 7 | #425 | feat(cli): doctor --capabilities, telemetry cost, probe max_tokens |
| 8 | #426 | refactor: round-2 audit fixes (drop 2 dead prompts, 3 dead TelemetryEvent, docs/spec-impl-gaps.md) |
| 9 | #427 | refactor: round-3 audit fixes (drop ledger, 5 error companion modules, tiktoken-rs) |
| 10 | #430 | feat(llm): wire models.dev gates into dispatch_to_provider (catalog refresh + ModalityGate + cost_estimate + CapabilityResolver) |
| 11 | #432 | fix(llm): pass capability-gated request to dispatch_to_provider (Closes #428 — adopted from the e2e-network loop's prepared fix at 75ce6b2) |
| 12 | #433 | refactor: round-4 audit (drop 5 telemetry modules: manifest_ext, manifest_txt, manifest_version, recover, phase_macro) |
| 13 | #434 | refactor: round-4 audit (drop invalidate_downstream, matrix_seed) |
| 14 | #435 | refactor: round-4 audit (drop 4 empty v013 tables via v016 migration) |
| 15 | #436 | refactor: round-5 audit (drop 5 dead modules + trim BudgetPolicy::Abort) |
| 16 | #437 | docs(audit): re-create round-1 + round-2 inconsistency audits |
| 17 | #438 | refactor: round-6 audit (drop unused anthropic_compat + streaming, 666 LoC) |
| 18 | #439 | refactor: round-6 audit (drop dead manifest_versions via v017 + fix stale docstring) |
| 19 | #440 | refactor: round-7 audit (drop unique_tempdir + 4 newly-orphaned items + refresh 4 stale docstrings) |

PR #421 (reasoning gating) was closed unmerged on 2026-08-12
— the merge with main required 100+ manual Request-literal
fixes that were too noisy for the time budget. The
`reasoning_gate.rs` helper survives on the
`feat/models-dev-reasoning-gate` branch and can be re-attempted
in a follow-up.

PR #424 (audit round 1, `fix/audit-findings`) was closed on
2026-08-13 — the round-1 audit fixes were subsumed by the
later rounds (#426 and #427).

## Session 2 (2026-08-13) summary

Between 04:40 UTC and 07:33 UTC (~3h), the **models-dev session
round 2** landed:

- Adopted the e2e-network loop's prepared fix (PR #432)
- 8 cleanup PRs (rounds 4-7) dropped ~1,100 LoC of dead code
  + 1 SQLite table (v016) + 1 SQLite table (v017)
- Re-created the audit reports that were lost on 2026-08-12
- 1846 → 1826 lib tests (-20 dead tests, 0 regressions)
- main HEAD: `a7c4655`

Final test count: 1826 lib + 30 integration, 0 failed.
Final main HEAD: `a7c4655`.

## Session 3 (2026-08-13) — round-10 + round-11 + round-12 audit closure

Between 09:35 UTC and 13:30 UTC (~4h), the **round-10 + round-11 + round-12 session**
landed the long-stranded PR #424 re-derivation + the long-stranded
Spanish identifier rename + 4 cleanup passes + docs closure + **discovery
e2e validation against `opencode_go`**:

### Round-10 PRs (6)

| PR | Subject | LoC | Round-1 / round-8 item closed |
|---:|---|---:|---|
| [#444](https://github.com/airvzxf/moagan/pull/444) | `refactor(cli): drop 7 dead env-var helpers + BatchPolicy + ROUTING_TOML_AVAILABLE stub` | -108 | round-1 §A.1 |
| [#445](https://github.com/airvzxf/moagan/pull/445) | `refactor(storage): drop dead FullLease wrapper (143 LoC)` | -143 | round-1 §A.5 |
| [#446](https://github.com/airvzxf/moagan/pull/446) | `refactor(discovery): rename Spanish identifiers to English` | 0 | round-1 §C.1 |
| [#447](https://github.com/airvzxf/moagan/pull/447) | `feat(cli): wire Config::token_budget into Db::set_budget at run start` | +94 | round-1 §E.1 row 9 |
| [#448](https://github.com/airvzxf/moagan/pull/448) | `docs(audit): mark round-10 closure for PRs #444-#447 + round-1 fully closed` | docs | docs closure |
| [#449](https://github.com/airvzxf/moagan/pull/449) | `refactor: drop 4 trivial pub fn with single test-only caller` | -71 | round-8 §E.4 borderline |

### Round-11 PRs (5)

| PR | Subject | LoC | Item closed |
|---:|---|---:|---|
| [#451](https://github.com/airvzxf/moagan/pull/451) | drop 2 dead modules (reconcile::per_run + sandbox::tool_versions) | -288 | 2 dead `pub mod` |
| [#452](https://github.com/airvzxf/moagan/pull/452) | drop 6 test-only 1-caller `pub fn` + 1 inline `llm::registry` | -386 | test-only fns + inline mod |
| [#453](https://github.com/airvzxf/moagan/pull/453) | drop 1 dead fn + demote 4 internal `pub fn` | -21 | with_cardinality_and_profiles + cardinality demotes |
| [#454](https://github.com/airvzxf/moagan/pull/454) | docs(coord) — round-11 closure | docs | docs closure |
| [#455](https://github.com/airvzxf/moagan/pull/455) | demote 5 internal `pub fn` visibility (judge + cluster + synthesize) | 0 | 5 misclassified "dead" fns |

### Round-12 PRs (3)

| PR | Subject | LoC | Item closed |
|---:|---|---:|---|
| [#456](https://github.com/airvzxf/moagan/pull/456) | `phases/` round-12 mass cleanup | -129 | HardIncompat::is_incompatible_with + DiscoverMatrixPhase::from_dimensions_with_profiles + cluster_text/write_skipped_in_dir demotions |
| [#457](https://github.com/airvzxf/moagan/pull/457) | `llm/` round-12 mass cleanup | -81 | BreakeredProvider::{breaker, rate_limiter, provider_semaphores, inner} + RateLimiter::last_wait + probe_table::{probes_succeeded, probes_failed, persist_to} + LazyApiKey::with_spec + 4 demotions |
| [#458](https://github.com/airvzxf/moagan/pull/458) | cross-module round-12 (redact+storage+domain+sandbox+cli) | -421 | inline `builtin_patterns` (137 LoC) + delete `is_incompatible_with` (30 LoC) + 6 demotions + 4 dead compression helpers |

### Discovery e2e (P8) — closed (PR #459)

| PR | Subject | Notes |
|---:|---|---|
| [#459](https://github.com/airvzxf/moagan/pull/459) | `feat(e2e): validate discovery mode with opencode_go + Token Plan` | Closes the long-blocked P8 gap. The operator's `OPENCODE_GO_API_KEY` in `.env` was valid all along — a `curl` to `https://opencode.ai/zen/go/v1/chat/completions` with `model=kimi-k2.7-code, temperature=1` confirmed the upstream works. Adds 88 LoC to `scripts/e2e_audit_proxy.sh` + 88 LoC new integration test (`#[ignore]`-gated). |

### Docs closure (PRs #448, #450, #454, #460)

5 docs PRs across the session tracking the closures.

**Net session 3**:
- 1819 → 1784 lib tests (-35 dead tests, 0 regressions).
- **~1,535 LoC of dead code removed** across 13 refactor PRs.
- 1 behavior wire-up (`token_budget → Db::set_budget`).
- 1 spec-compliant rename (Spanish → English across `discovery/`).
- Round-1 audit **fully closed**.
- Round-8 actionable list **9 of 10 closed** + 1 partial (persona_angle
  misclassified — actually alive).
- **P8 discovery validation closed** with real `OPENCODE_GO_API_KEY` from `.env`.
- **PR #462 follow-up**: added the native `deepseek` provider e2e
  validation using `DEEPSEEK_API_KEY` directly (NOT routed through
  opencode.ai). Closes the parallel e2e gap for the second LLM
  provider kind shipped in v0.6.
- main HEAD: `7facb6c`.

**5 misclassified "dead" fns in `phases/`**: a research subagent correctly
stopped before deleting them. They have production callers via
`JudgePhase::run` and `SynthesizePhase::run`. Demoted `pub fn` → `fn`
in PR #455 instead.

Final test count: **1784 lib + 30 integration, 0 failed**.
Final main HEAD: `7facb6c`.

_Last updated: 2026-08-13 13:30 UTC_

## Branch inventory (live)

| Branch | Owner | Status |
|---|---|---|
| (none — all PRs merged) | | |

_Last updated: 2026-08-13 10:19 UTC_
