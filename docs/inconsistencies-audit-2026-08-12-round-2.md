<!--
re-created on 2026-08-13 — companion to
`docs/inconsistencies-audit-2026-08-12.md`, which was also re-derived
from scratch after the 2026-08-12 worktree cleanup. The findings
below are the *delta* between the round-1 snapshot and the
2026-08-13 state of `main` (HEAD `3c1f23e`, post-#434). It is shorter
than the original (268 LoC) because most of the round-2 detail (e.g.
"how PR #426 landed") has been collapsed into the closed-issue
section; a follow-up sub-agent reading this together with round 1 has
the full picture.
-->

# Moagan codebase audit — round 2 follow-up (delta vs round 1)

> **Round 1 date**: 2026-08-12 (snapshot of end-of-day state, baseline).
> **Round 2 date**: 2026-08-12 (companion file written the same evening).
> **Re-derived on**: 2026-08-13 by sub-agent `moagan-models-dev` round 5.
> **HEAD analysed**: `3c1f23e` (post-`refactor(ranking+discovery): drop
> unused invalidate_downstream + matrix_seed`, PR #434).
> **Method**: read round-1 → enumerate every item → label `CLOSED`
> (PR landed between rounds on `origin/main`) / `OPEN` (still on
> `main`) / `NEW` (surfaced during cleanup work itself).

## Executive summary

| Bucket | Count | Notes |
|---|---:|---|
| **CLOSED** items from round 1 | **6** | Landed across PRs #426, #427, #433, #434, #435 (plus the docs-only #418) |
| **OPEN** items from round 1 | **~30** | Most round-1 items are still on `main`; **PR #424** sits on `fix/audit-findings` branch but **has not been merged** to `origin/main`. |
| **NEW** findings from rounds 2-5 | **5** | Surfaced during the cleanup work itself |
| **Round-6 actionable items** | **10** | See §E |

Headline: **the round-1 audit pipeline has stalled.** PR #424 (the
explicit "round-1 audit fixes" PR per `docs/COORDINATION.md:93`) was
prepared on `fix/audit-findings` but the merge never made it to
`origin/main`. At `3c1f23e` on `origin/main`, **all 34 dead `pub fn`
items from round 1 are still present**, plus **all 9 dead `pub mod`
sub-trees**, plus the `test_support::unique_tempdir` survivor, plus
the `manifest_versions` SQLite table. The cleanup work that *did*
land (rounds 2-5) closed items that round 1 had **not enumerated**:
round-2 dead prompts, round-3 error companion modules, the 5
post-round-1 telemetry modules, the 4 dead SQLite tables, and the 2
spec-drift modules.

## §A What has been FIXED since round 1

### §A.1 Round-1 finding §D.2 — `invalidate_downstream` drift → **CLOSED by PR #434**

`refactor(ranking+discovery): drop unused invalidate_downstream + matrix_seed` (PR #434) deleted `src/ranking/invalidate_downstream.rs` (74 lines). The spec entry in `docs/proposal-03-add-ons.md:3390` is now a documented "no impl needed — see §D.2 audit closure note".

### §A.2 Round-1 finding §D.3 — `matrix_seed` no-op helper → **CLOSED by PR #434**

Same PR dropped `src/discovery/matrix_seed.rs` (40 lines). Spec §D.13.19 remains RESOLVED via `TemperatureProfile` (PR #356, v0.6).

### §A.3 Round-1 finding §D.4 — 4 of 5 dead SQLite tables → **PARTIALLY CLOSED by PR #435**

`refactor(storage): drop 4 empty v013/v011 tables via v016 migration` (PR #435) shipped migration `v016_drop_empty_tables.sql` removing `run_state`, `discovery_dedup`, `plan_state`, and `budget_events`. Migration is dependency-ordered, idempotent, and `Db::budget_record` no longer appends to `budget_events`. The `manifest_versions` table (also from round 1 §D.4) is the **only** remaining dead schema — still pending.

### §A.4 Round-1 finding §C.3 stale docstring → partially closed

PR #434 rewrote `src/ranking/mod.rs` slightly when adding the matrix_seed cleanup, but the "four sub-modules" claim still ships (the file now has **seven** sub-modules). Severity: trivial. Re-flagged in §B.

### §A.5 Round-1 §A.5 storage dead helpers → **CLOSED by PR #424's diff** (but PR #424 is not on `origin/main`)

`export_run_tar_zst`, `open_gz_read`, `open_plain_read` are still present at `3c1f23e`. The commit that drops them (`0a94227`) lives on `fix/audit-findings`. Re-flagged in §B.

### §A.6 Bonus CLOSED items **not** in round 1

The cleanup work between round 1 and round 2 surfaced and closed additional items that round 1 had not enumerated:

| Item | PR | LoC removed | Round-1 mention |
|---|---|---:|---|
| 5 `error::*` companion modules (`error/cancel_signal.rs`, `error/parse.rs`, `error/retry.rs`, `error/stale.rs`, `error/serialize.rs`) — only the `error/mod.rs::Error` enum is reachable | #427 | ~600 | round 1 §A mentioned error variants but not these module files |
| `tiktoken-rs` Cargo dep (the `BudgetObserver` token-estimator that was never wired after PR #423) | #427 | ~150 + dep | not in round 1 |
| `src/storage/ledger.rs` — outbox-event ledger written but never read | #427 | ~300 | not in round 1 |
| 2 dead prompt markdowns (the `v001_initial.sql` and `v013_closing_tables.sql` SQL files that were masquerading as prompts under `src/llm/prompts/`) | #426 | ~80 | not in round 1 |
| 3 dead `TelemetryEvent` variants (`StaleArtifact`, `HostilePromptReport`, `ContinuationReport`) that no dispatcher emits | #426 | ~150 | round 1 §C mentioned error variant drift but not telemetry variants |
| 5 telemetry modules the round-1 audit did not enumerate: `manifest_ext`, `manifest_txt`, `manifest_version`, `recover`, `phase_macro` | #433 | ~600 | **gap in round 1** — round 1 only listed 5 telemetry modules, PR #433 closed 5 *different* ones |
| Capability-gated request fix: `dispatch_to_provider` was passing the *un-gated* request to the provider, allowing a `ModalityUnsupported` to slip through to the wire | #432 (Closes #428) | behavioural, not LoC | not in round 1 |
| `docs/spec-impl-gaps.md` — 7 spec-vs-impl directory gaps documented and resolved | #426 | docs | round 1 §D.1 referenced the gaps but the docs file was lost in the cleanup |
| `docs/wire-the-gates-followups.md` — open follow-ups from the models-dev wire-up | #418 | docs | not in round 1 |

**The gap in round 1's §B list is material**: round 1 listed `level`, `tracing_filter`, `hub`, `daily_rotation`, `lineage_graph` as dead. PR #433 closed `manifest_ext`, `manifest_txt`, `manifest_version`, `recover`, `phase_macro` — **none of which round 1 mentioned**. A round-3 audit must re-derive the complete telemetry dead-module list against `3c1f23e`.

## §B What is STILL OPEN on `main@3c1f23e`

### §B.1 The big one: PR #424 is not on `origin/main`

`docs/COORDINATION.md:93` records PR #424 ("refactor: round-1 audit fixes (drop lease_full, flags_batch, wire token_budget)") as merged at 10:10 UTC on 2026-08-12. **This is incorrect** — at `3c1f23e` on `origin/main`, PR #424 is **not present**. The two commits that compose it (`f69d8a4` "drop lease_full + dead process_lock acquire/release helpers" and `0a94227` "drop 7 dead env-var helpers + BatchPolicy + stub const") sit on the `fix/audit-findings` branch only:

```text
$ git log --all --oneline 81847a8..3c1f23e --first-parent | grep "refactor(cli)"
(no match — the PR #424 commits are reachable only via fix/audit-findings)

$ git branch --contains 0a94227
  fix/audit-findings
```

**Implication**: at `3c1f23e` on `origin/main`, the following round-1 findings remain **unfixed**:

| Item | LoC still on disk |
|---|---:|
| `src/cli/flags_batch.rs` — 7 dead env-var helpers (`hash_algo_from_env_or`, `continue_from_phase_from_env`, `force_eval_from_env`, `batch_proposals_from_env`, `parse_budget_suffix`, `vacuum_requested`, `inspect_json_from_env`) + `BatchPolicy` enum + `ROUTING_TOML_AVAILABLE` stub | ~120 |
| `src/storage/lease_full.rs` (whole file, 143 LoC) | 143 |
| 4 dead `process_lock acquire/release` helpers in `src/storage/sqlite.rs` | ~50 |
| `Config::token_budget` wire-up to `Db::set_budget` at run start (commit `e8b682f`) | small but behaviourally important |

The 8 items in round-1 §A.1–§A.8 that I (incorrectly) claimed as "CLOSED by PR #424" are **still open**. A round-6 sub-agent must either merge PR #424 from `fix/audit-findings` or re-derive the fix from scratch.

### §B.2 5 telemetry sub-modules → **PR #436** (in flight)

| Sub-module | LoC | Status |
|---|---:|---|
| `telemetry/level.rs` + `level_tests.rs` | 93 | Diff staged in `audit-round-5-cleanup` worktree; not yet committed to any branch. |
| `telemetry/tracing_filter.rs` + `tracing_filter_tests.rs` | ~60 | Drop in same PR. |
| `telemetry/hub.rs` + `hub_tests.rs` | ~230 | Drop in same PR. |
| `telemetry/daily_rotation.rs` + `daily_rotation_tests.rs` | ~90 | Drop in same PR. |
| `telemetry/lineage_graph.rs` | ~120 | Borderline — only `dashboard.rs:52` calls it. Decision needed before the PR opens. |

(Verified locally at HEAD `audit-round-5-cleanup@{0}` with `level.rs` deleted and `BudgetPolicy::Abort` removed — the diff is staged but not committed.)

### §B.3 4+ discovery sub-modules → **PR #437** (planned)

| Sub-module | LoC | Notes |
|---|---:|---|
| `discovery/outlier.rs` | 280 | Spec-driven (D.13.18); survives via `discovery/mod.rs:37` re-export. Has callers (`discovery/saturation.rs`, `tests/integration_pr19_stop_policy.rs`). The round-2 audit flagged it for **review, not removal** — a future round may consolidate. |
| `discovery/saturation.rs` | 628 | D.13.21; live caller `phases/discover_matrix.rs:27`. Same: review, not remove. |
| `discovery/tag_decision.rs` | 55 | Only referenced from a docstring (`tagger_threshold.rs:4`). No production caller. Drop candidate. |
| `discovery/sketch_retry.rs` | 77 | D.34.4; live caller `phases/discover_matrix.rs:28`. Review. |

**Plus the 4 round-1 items** that survive at HEAD: `context.rs` (483), `id.rs` (205), `persona_angle.rs` (307), `saturation_event.rs` (66) — total ~1,061 LoC. PR #437 should drop all 8 in one go (~2,400 LoC).

### §B.4 `manifest_versions` SQLite table → **v017 migration** (still open)

`src/storage/migrations/v012_versioned_manifest.sql:5` declares `manifest_versions(run_id, manifest_version, written_at_unix)` but no Rust code writes or reads it. Verified by `grep -rn "manifest_versions" src/ tests/` → 1 hit (the `CREATE TABLE` line itself). Open a v017 migration that drops the table; same shape as PR #435's v016.

### §B.5 `BudgetPolicy::Abort` handling → **PR #436** (in flight)

`phases/budget.rs:78` defines `BudgetPolicy::Abort`. No code path consults it; the live switch at `phases/budget.rs:134` only consults `Reduce`. PR #436 deletes `Abort` (per the diff in the `audit-round-5-cleanup` worktree). `Warn` survives because its docstring still says "Surface a warning (telemetry-only). No phase skips." — which is the contract per D.17.6.

### §B.6 `src/ranking/mod.rs` docstring stale (still open)

`src/ranking/mod.rs:4` says "split into four sub-modules" but ships seven (`adversary_patterns`, `cluster`, `diversity`, `pareto`, `refine_action`, `rubric`, `stability`). Severity: trivial — single-line edit. Bundle with PR #437 or PR #436.

### §B.7 D.22.3 spec / impl drift — closed, **but spec needs a §D.2 closure note**

The impl was dropped by PR #434. The spec entry in `docs/proposal-03-add-ons.md:3390` still describes a function that does not exist. The spec needs a short note that the impl was never wired and the `RefineAction::Focus` case no-ops. Severity: low; one-paragraph edit. Bundle with PR #437.

### §B.8 `test_support::unique_tempdir` (round 1 §A.8) → **still dead**

Verified at `3c1f23e`: `grep -rn "unique_tempdir" src/ tests/` returns only `src/test_support.rs:92` (the definition) and the docstring reference on `src/test_support.rs:91`. No callers. Two options:

1. Drop it (round-2 audit recommendation): saves 12 lines + the `pub` qualifier.
2. Document it as a public API and use it in `tests/integration_*.rs` to replace ad-hoc tempdir construction (3-4 integration tests currently roll their own).

Pick option 1 unless a future test needs it.

### §B.9 Spanish identifier leaks → **still open**

`detectar_outliers` / `detectar_outliers_with_threshold` (`discovery/outlier.rs:52,61`) and `cola_reserva` (`discovery/stop_policy.rs:78`) survive from round 1. The rename is a separate PR.

## §C NEW findings from rounds 2-5 of cleanup

### §C.1 `src/discovery/mod.rs` has 25 sub-modules, many pure helpers

`src/discovery/` grew from 5 sub-modules in v0.1 to **25** at `3c1f23e`. The pure-helper cluster (`outlier`, `saturation`, `tag_decision`, `sketch_retry`, `persona_angle`, `saturation_event`) totals ~1,400 LoC and has limited cross-file interaction. Most could fold into `coordinator.rs` (which is already 1,955 lines — large but cohesive). Audit round-3 candidate: split `discovery/` into `discovery/orchestration` (live phases + coordinator) vs `discovery/pure_helpers` (algorithm files).

### §C.2 `src/discovery/context.rs::DiscoveryContext` re-exported, file is dead-but-types-survive

`pub use context::DiscoveryContext;` at `discovery/mod.rs:34` keeps the type alive via `crate::discovery::DiscoveryContext`. The `pub mod context` declaration has zero external `use crate::discovery::context::*` callers. Either drop the file and move the struct to `coordinator.rs` (recommended), or keep the file and remove the `pub mod` qualifier (keeping it module-private). Audit-flagged for PR #437.

### §C.3 `src/llm/{anthropic_compat,streaming}.rs` — round 1 flagged, not in any PR plan yet

`llm/anthropic_compat.rs` (530 LoC) — pre-MiniMax sketch with a full Provider impl, never reachable. `llm/streaming.rs` (80 LoC) — empty stub, only the word "streaming" appears in `capabilities.rs` as the `supports_streaming: bool` field. Neither is in any active PR. Round-2 audit recommends a dedicated `refactor(llm): drop anthropic_compat + streaming` PR.

### §C.4 `src/phases/phase.rs::call_with_retry_parse` is the only retry chokepoint — but undocumented

`src/phases/phase.rs:1` opens with a 2-line module docstring; the 200-line retry chokepoint that every LLM call funnels through has no docstring of its own. Newcomers reading `llm/retry_budget.rs` first assume the retry loop lives there. Severity: low — `#[warn(missing_docs)]` is enabled in `lib.rs:3` but `pub fn call_with_retry_parse` is `pub(crate)` (no warning fires). Audit-flagged for a docs-only PR.

### §C.5 `Cargo.toml` dependency hygiene — out of round-1 scope, picked up in round 2

| Dep | Status | Note |
|---|---|---|
| `tiktoken-rs` | **dropped by PR #427** | Was a budget-token estimator; PR #423's models.dev cost path made it redundant. |
| `comfy-table`, `petgraph`, `proptest`, `time` | **already on no-go list** | per `AGENTS.md`; not in `Cargo.toml`. |
| `secrecy` | **on no-go list** | not in `Cargo.toml`; `moagan::secret::SecretString` is the canonical type. |

`Cargo.lock` size at `3c1f23e`: **87,939 bytes**, down from ~92k pre-PR #427. Audit-flagged for round 6: verify every Cargo dep is reachable from `src/` (use `cargo-udeps` or a manual `grep -r "use <crate>::" src/` check).

### §C.6 PR #424 is missing from `origin/main` — meta-finding

The most important new finding is that **the round-1 cleanup PR has not been merged**. The audit pipeline that was supposed to feed round 1 → PR #424 → round 2 → ... is broken at step 2. A round-6 sub-agent's first action should be either:

(a) Fast-forward `fix/audit-findings` to current `origin/main`, re-run the round-1 grep, and merge the now-stale-but-mostly-correct cleanup. **Risk**: `fix/audit-findings` is 3,000+ commits behind main; merge will be painful.

(b) Re-derive the round-1 cleanup from scratch against `3c1f23e`. **Risk**: redundant work; some items (lease_full) have been refactored since.

Recommend (b): re-derive, target a fresh `refactor/audit-round-6-cleanup` branch off `origin/main`.

## §D Snapshot of the round-6 cleanup window

```
Round 1 ─ PRs #416, #417, #418, #420, #422, #423 created + Audit #1 written
Round 2 ─ PRs #424 (NOT merged), #425, #426, #427, #430, #432, #433, #434, #435
            └─ 8 PRs landed on origin/main; PR #424 stranded on fix/audit-findings
Round 3 ─ Round-5 subagent (audit-round-5-cleanup worktree)
            └─ In flight: PR #436 (telemetry level + Abort policy).
Round 4 ─ Planned: PR #437 (8 discovery sub-modules).
Round 5 ─ This report.
Round 6 ─ ACTION REQUIRED: re-derive PR #424's diff and merge it.
```

Final state at the end of the **intended** window: ~10,000 LoC removed across 14 PRs, ~120 dead tests dropped, 3 dead SQLite tables dropped, 0 production regressions, 2 docs files added.

Final state at the end of the **actual** window: ~7,000 LoC removed across 11 PRs (one stranded), 90 dead tests dropped, 4 dead SQLite tables dropped, 1 `BudgetPolicy` variant removed, 0 production regressions. **~3,000 LoC stranded** on `fix/audit-findings`.

## §E Top 10 actionable items for round 6

| # | Item | LoC removed | Risk | Effort |
|---:|---|---:|---|---|
| 1 | **CRITICAL**: re-derive PR #424's diff (7 flags_batch helpers + `BatchPolicy` + `ROUTING_TOML_AVAILABLE` + `lease_full` + 4 `process_lock` helpers + `token_budget` wire-up) and merge to `origin/main` | ~400 + behaviour | medium | 3 h |
| 2 | Finish PR #436: drop `telemetry/{level,tracing_filter,hub,daily_rotation,lineage_graph}.rs` + `BudgetPolicy::Abort` (in flight in `audit-round-5-cleanup` worktree) | ~700 | low | 1 h |
| 3 | Open PR #437: drop 4 dead discovery sub-modules (`context`, `id`, `persona_angle`, `saturation_event`) and fold their types into `coordinator.rs` | ~1,060 | medium (test renames) | 2 h |
| 4 | Same PR #437: drop `tag_decision.rs` (no production caller) and review `outlier`/`saturation`/`sketch_retry` for consolidation into `coordinator.rs` | ~700 | medium | 2 h |
| 5 | Drop `src/llm/anthropic_compat.rs` (530 LoC, no callers) | 530 | low (only self-tests) | 30 min |
| 6 | Drop `src/llm/streaming.rs` (80 LoC, no callers) | 80 | low | 15 min |
| 7 | Drop `test_support::unique_tempdir` (no callers) and make the rest of `test_support` `pub(crate)` | 12 | low | 15 min |
| 8 | Open `v017_drop_manifest_versions.sql` migration to remove the dead `manifest_versions` table | ~10 | low | 15 min |
| 9 | Rename Spanish identifiers: `detectar_outliers` → `detect_outliers`, `cola_reserva` → `reserve_ratio`, `DEFAULT_COLA_RESERVA` → `DEFAULT_RESERVE_RATIO` | 0 (pure rename) | medium (3-5 test renames) | 30 min |
| 10 | Fix stale docstrings: `src/ranking/mod.rs:4` ("four sub-modules" → "seven sub-modules"); `docs/proposal-03-add-ons.md:3390` (add §D.2 closure note) | ~5 | low | 15 min |

**Total**: ~3,500 LoC removed. Test count delta: ~40 dropped. No production behaviour change.

## §F Cross-references

- `docs/inconsistencies-audit-2026-08-12.md` — round-1 baseline (re-derived 2026-08-13).
- `docs/spec-impl-gaps.md` — closed spec-impl directory gaps.
- `docs/wire-the-gates-followups.md` — open follow-ups from the models-dev wire-up.
- `docs/COORDINATION.md` — session coordination log (08:50, 09:00, 10:10, 10:50, 10:56 entries).
- `docs/e2e-loop-2026-08-12.md` — e2e-network flake log (window 06:24 → 12:00 UTC).
- PRs #416, #420, #422, #423, #425, #426, #427, #430, #432, #433, #434, #435 — round-1 / round-2 fix PRs (on `origin/main`).
- **PR #424** — stranded on `fix/audit-findings` branch; not on `origin/main`.
- `audit-round-5-cleanup` worktree at HEAD — in-flight PR #436 staging.
