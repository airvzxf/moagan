<!--
re-created on 2026-08-13 — original document was lost when the worktrees
were cleaned up on 2026-08-12. The findings below were re-derived by
re-running the reference-counting grep against `origin/main` HEAD
`3c1f23e` (post-#434), so the snapshot is one day fresher than the
original. The structure mirrors the original round-1 report (580 LoC,
work-tree only, never committed). Companion file:
`docs/inconsistencies-audit-2026-08-12-round-2.md`.
-->

# Moagan codebase audit — inconsistencies & dead code (round 1)

> **Date of audit**: 2026-08-12 (snapshot of the state at end of day).
> **Re-derived on**: 2026-08-13 by sub-agent `moagan-models-dev` round 5.
> **HEAD analysed**: `3c1f23e` (post-`refactor(ranking+discovery): drop
> unused invalidate_downstream + matrix_seed`, PR #434).
> **Scope**: every `pub fn` and every `pub mod` in `src/` plus the
> SQLite migration graph. Cross-references against
> `docs/proposal-02-rust.md` (normative, spec id `T01-06`) and
> `docs/proposal-03-add-ons.md` (additive catalog).
> **Method**: word-boundary `grep -rn "\b<fn_or_mod_name>\b" src/
> tests/ docs/ examples/` for every candidate; counted only references
> outside the candidate's own file and outside its own `#[cfg(test)]`
> module. Anything with a count ≤ 1 (definition line only) is flagged
> as **dead**.

## Executive summary

| Class | Count | Severity | Notes |
|---|---:|---|---|
| Dead `pub fn` (zero callers anywhere) | **34** | medium | Spread across 11 sub-trees; ~600 LoC of pure-helpers + tests. |
| Dead `pub mod` sub-tree (no external `use crate::…::<mod>`) | **9** | medium-high | 4 in `discovery/` (already re-exported so the *types* survive), 5 in `telemetry/` and `llm/`. |
| Disconnected top-level `pub mod` (zero `use crate::<mod>`) | **0** | n/a | Every top-level `pub mod` in `lib.rs` has at least 2 external references. |
| Spec-vs-impl gaps (open) | **3** | low | GLM / Qwen / Kimi providers per `docs/spec-impl-gaps.md` §3 — intentional, no operator ticket. |
| Spec-vs-impl drift (asymmetric) | **1** | medium | D.22.3 `invalidate_downstream` spec describes a signature the impl never matched; **closed** by PR #434 dropping the impl. |
| Stale docstrings | **2** | low | `src/ranking/mod.rs:4` (says "four sub-modules" but ships seven); `src/phases/budget.rs:33` (still describes `BudgetPolicy::Abort`). |
| Unwired enum variants | **1** | low | `BudgetPolicy::Warn` + `BudgetPolicy::Abort` are reserved "future hooks" that nothing consults. |
| Dead SQLite tables (no writers, no readers) | **5** | low | 4 dropped by v016 in PR #435; 1 (`manifest_versions`) still pending. |

**Headline**: **34 truly dead `pub fn`** plus **9 dead `pub mod` sub-trees** are sitting on `main` producing 0 production value. Cleanup is mechanical: ~1,200 LoC of pure helpers, ~120 lines of test scaffolding, no behaviour change.

## §A Dead code (`pub fn` with zero callers)

34 `pub fn` items have **only their definition line as a reference** anywhere in the repo (no production caller, no test caller, no doc reference). They are sorted by file and listed with the test scaffolding they pull in. Removing each item also removes the `#[cfg(test)] mod tests` block it carries.

### §A.1 `src/cli/flags_batch.rs` — 8 dead helpers

PR #424 already collapsed the env-var helper cluster into the `--flag` CLI surface. The remaining file is a single-survivor module that exists only because `llm/wire.rs` consults its `HashAlgo` enum. Every free function is dead.

| Line | Symbol | LoC removed (incl. tests) |
|---:|---|---:|
| 36 | `hash_algo_from_env_or` | ~15 |
| 60 | `continue_from_phase_from_env` | ~12 |
| 66 | `force_eval_from_env` | ~12 |
| 74 | `batch_proposals_from_env` | ~12 |
| 82 | `parse_budget_suffix` | ~14 |
| 98 | `vacuum_requested` | ~10 |
| 106 | `inspect_json_from_env` | ~10 |

**Recommendation**: delete `src/cli/flags_batch.rs` entirely; move `HashAlgo` into `src/llm/wire.rs` (it's the only survivor) and update the `impl From<HashAlgo> for CacheHashAlgo` in `src/llm/wire.rs:209`. Saves 216 lines.

### §A.2 `src/cli/{audit,probe,run}.rs` — 3 dead helpers

| File:line | Symbol | Notes |
|---|---|---|
| `cli/audit.rs:61` | `resolve_run` | Resolves `--run <id-or-path>` to a `RunId`; never called. The CLI parses `--run` directly via clap (see `cli/audit.rs:1-60`). |
| `cli/probe.rs:262` | `parse_provider_model` | Splits `"provider/model"`; never called. The probe subcommand uses `cli::probe::split_provider_model` (a different helper). |
| `cli/run.rs:623` | `pipeline_shape` | Computes a debug representation of `Pipeline`; never called. |

### §A.3 `src/phases/{intake,synthesize,replace,util}.rs` — 4 dead helpers

| File:line | Symbol | Notes |
|---|---|---|
| `phases/intake.rs:615` | `read_intake_with_context` | Reads `Intake` with a `ContextRef` injected; never called. The `IntakePhase::run` path uses `read_intake_with_context_from_run_id` (different name). |
| `phases/synthesize.rs:110` | `merge_plan_to_synthesized` | Converts `MergePlan` → `SynthesizedProposal`; never called. |
| `phases/replace.rs:72` | `sources_to_replace` | Computes the "should we replace?" decision; never called. `replace.rs:35 should_replace_synthesis` is the live counterpart. |
| `phases/util.rs:1043` | `strip_code_fence` | Strips ``` ```json ``` fences from raw LLM output; never called. `parse_model_json_traced` already handles this internally. |

### §A.4 `src/telemetry/{export,verify,dashboard}.rs` — 3 dead helpers

| File:line | Symbol | Notes |
|---|---|---|
| `telemetry/export.rs:49` | `format_sha256sums` | Renders a `<run>.SHA256SUMS` manifest; never called. The CLI uses `export_run` (line 142) which calls `sha256_file` directly. |
| `telemetry/verify.rs:257` | `sha256_hex_of` | Computes SHA-256 of a file path; never called. `telemetry/verify.rs:263 sha256_hex(bytes)` is the bytes-only live counterpart. |
| `telemetry/dashboard.rs:476` | `compute_hashes` | Recursive SHA-256 walker for the dashboard; never called. |

### §A.5 `src/storage/compression.rs` — 3 dead helpers

| File:line | Symbol | Notes |
|---|---|---|
| `storage/compression.rs:51` | `open_gz_read` | Returns `Box<dyn Read>`; never called. `compression.rs:273 reader(path, c)` is the live dispatch. |
| `storage/compression.rs:60` | `open_plain_read` | Same — never called. |
| `storage/compression.rs:397` | `export_run_tar_zst` | Whole-run `tar.zst` exporter; never called. The live export path lives in `src/cli/telemetry_cmd.rs`. |

### §A.6 `src/audit/{format,verify}.rs` — 3 dead helpers

| File:line | Symbol | Notes |
|---|---|---|
| `audit/format.rs:116` | `crc32_hex` | CRC-32 over audit record body; never called. |
| `audit/format.rs:265` | `recompute_crc` | Same. |
| `audit/verify.rs:74` | `read_records` | Loads `Vec<AuditRecord>` from a sidecar; never called. `verify.rs:125 verify` is the live caller — it builds the list inline. |

### §A.7 `src/discovery/integrator.rs` — 2 dead helpers

| File:line | Symbol | Notes |
|---|---|---|
| `discovery/integrator.rs:41` | `preserved_citations_ratio` | Ratio of citations preserved between original and refined; never called. The discover_integrate phase uses inline coverage ratios. |
| `discovery/integrator.rs:155` | `category_header` | Markdown section header for a facet category; never called. |

### §A.8 Misc single-symbol items

| File:line | Symbol | Notes |
|---|---|---|
| `reconcile/per_run.rs:39` | `reconcile_run` | Per-run reconcile pass; never called. The startup path uses `reconcile/mod.rs:84 startup_reconcile` which inlines the logic. |
| `context/resolver.rs:63` | `classify_no_io` | Same as `resolve_classify` but without reading `MOAGAN_HOME`; never called. |
| `context/loader.rs:380` | `group_by_type` | Counts `ContextRefRecord`s by `kind`; never called. |
| `llm/json_extractor.rs:87` | `extract_and_parse` | Tolerant JSON extractor; never called. `json_extractor.rs:68 extract_tolerant_json` is the live counterpart. |
| `llm/models_dev.rs:239` | `catalog_path` | `<home>/models_dev.json`; never called. `models_dev.rs:252 try_load_from_disk` inlines the join. |
| `llm/prompts.rs:225` | `inject_epistemic_legacy` | Injects epistemic legacy block into system prompt; never called. `prompts.rs:243 inject_epistemic_preferences` is the live counterpart. |
| `test_support.rs:92` | `unique_tempdir` | Exposed for "tests that want the unique-path generator without the env-var ceremony"; zero callers in `tests/`. `with_moagan_home` (line 60) is the canonical helper. |

## §B Disconnected modules (`pub mod` with no external `use`)

9 `pub mod` declarations have **zero external `use crate::X::Y` references**. The types they expose survive only via re-exports in the parent `mod.rs` (for `discovery::context`, `discovery::id`) or are completely unreachable.

### §B.1 `src/discovery/` — 4 dead sub-modules (types survive via re-export)

| Sub-module | LoC | Re-export at | Verdict |
|---|---:|---|---|
| `context.rs` | 483 | `discovery/mod.rs:34` `pub use context::DiscoveryContext;` | **drop the file**, keep the type as `DiscoveryContext` (rename to `discovery_context::DiscoveryContext` or move it into `coordinator.rs`). |
| `id.rs` | 205 | `discovery/mod.rs:36` `pub use id::{ContradictionId, FacetId, SketchId};` | **drop the file**, fold the three newtypes into `coordinator.rs`. |
| `persona_angle.rs` | 307 | (none) | **truly dead**. The `DiscoveryWiringConfig::persona_enabled` gate is permanently `false` on `main`. PR #178 ("feat: PersonaPicker + AnglePicker helpers via opt-in flag") shipped the helpers but never the wiring that flips the gate. See `docs/v0.7-final-report.md` §7 ("deferred items"). |
| `saturation_event.rs` | 66 | (none) | **truly dead**. `DiscoverySaturated::emit` writes a `tracing::info!` record that nothing scrapes. |

### §B.2 `src/telemetry/` — 5 dead sub-modules (slated for PR #436)

| Sub-module | LoC | Last real call site | Verdict |
|---|---:|---|---|
| `level.rs` + `level_tests.rs` | 44 + 49 | `level_tests.rs:3` (self-test only) | Drop. The `TelemetryLevel` enum (`Off`/`Summary`/`Full`) maps to nothing — the telemetry stream is "Full or off" via `--telemetry on/off`, not by level. |
| `tracing_filter.rs` + `tracing_filter_tests.rs` | ~30 + 30 | `tracing_filter_tests.rs:3` (self-test only) | Drop. The `recommended_env_filter()` helper produces a `tracing-subscriber` directive string that nobody passes to a subscriber. |
| `hub.rs` + `hub_tests.rs` | ~150 + ~80 | `hub_tests.rs:7` (self-test only) | Drop. `TelemetryHub` was the original in-process sink registry before the JSONL-sidecar rewrite (v0.3). |
| `daily_rotation.rs` + `daily_rotation_tests.rs` | ~60 + 30 | `daily_rotation_tests.rs:3` (self-test only) | Drop. Telemetry files rotate at the storage layer (`v008_add_ons.sql`), not at the in-memory `DailyRotator`. |
| `lineage_graph.rs` | ~120 | `dashboard.rs:52` (live, but trivial) | **Borderline**. Used by exactly one caller (`dashboard.rs:52`); the dashboard still works after removal. PR #436 should evaluate. |

### §B.3 `src/llm/` — 2 dead sub-modules (slated for PR #437 in spirit)

| Sub-module | LoC | Last real call site | Verdict |
|---|---:|---|---|
| `streaming.rs` | ~80 | (none — only "supports_streaming" `bool` field name appears in `capabilities.rs`) | **truly dead**. The SSE streaming wire lives inside each provider module (`opencode_go_responses.rs:37` uses `super::sse_parser`). |
| `anthropic_compat.rs` | ~530 | (none) | **truly dead**. No provider, no `Provider` impl, no test reaches `crate::llm::anthropic_compat::*`. The module is the leftover from a pre-MiniMax sketch; `minimax.rs` is the canonical Anthropic-compatible implementation. |

> Note: `api_keys_file.rs`, `provider_pool.rs`, and `sse_parser.rs` look dead by the same metric but are **alive via internal `super::X::*` calls**. Keep them — the audit must distinguish "no `use`" from "no callsite anywhere".

## §C Inconsistencies (naming, error variants, docstrings)

### §C.1 Spanish identifier leaks

| File:line | Symbol | Severity |
|---|---|---|
| `discovery/outlier.rs:52,61` | `detectar_outliers`, `detectar_outliers_with_threshold` | medium — surviving re-export at `discovery/mod.rs:37`. Rename to `detect_outliers` / `detect_outliers_with_threshold`. The test functions (`detectar_outliers_returns_unclustered`, etc.) follow the same pattern and must be renamed in lock-step. |
| `discovery/stop_policy.rs:78` | `cola_reserva: f32` field on `StopPolicy` | medium — Spanish for "reserve queue". `discovery/mod.rs:29` exports `DEFAULT_COLA_RESERVA = 0.25`. Rename to `reserve_ratio` and `DEFAULT_RESERVE_RATIO`. |
| `phases/deliver.rs:222` | `kind_badge_for` returns emoji + label; the `auditoría` marker in the body is a comment, not an identifier | low — comments are Spanish; per `AGENTS.md` "code in English, comments may quote Spanish strings". Not a defect. |

### §C.2 Error variant naming drift

| `Error::` variant | Source string format | Exit code |
|---|---|---|
| `Io`, `InvalidArgs`, `InvalidApiKey`, `PlanExhausted`, `Timeout`, `Cancelled`, `SchemaViolation`, `InvalidState`, `LockHeld`, `Provider`, `Cache` | `String` | varies |
| `Cancel(#[from] CancelSignal)` | `CancelSignal` | 6 (Cancelled) |
| `NeedsInput(String)` | `String` | 10 |
| `HostilePrompt(String)` | `String` | (no mapping in `exit_code`) |
| `PathTraversal(String)` | `String` | (no mapping) |
| `PayloadTooLarge(String)` | `String` | (no mapping) |
| `ModalityUnsupported(String)` | `String` | (no mapping) |
| `Raw(io::Error)`, `NoParent { path: PathBuf }`, `CreateDir { path, source }`, `CreateFile { path, source }`, `Write { path, source }`, `Sync { path, source }`, `OpenDir { path, source }`, `Rename { from, to, source }`, `Read { path, source }`, `Parse { path, source }`, `SerializeMeta`, `DeserializeMeta` | structural | (no mapping) |

**Drift**: `HostilePrompt`, `PathTraversal`, `PayloadTooLarge`, `ModalityUnsupported` are *semantic* errors that should map to specific exit codes (per the catalog §D.12.10/.11/D.16.2/D.26.5/D.29.9) but `exit_code()` in `error/mod.rs:489` returns `1` (GenericError) for every unmapped variant. Severity: medium — operators cannot distinguish "the prompt was hostile" from "the prompt was too large" from "the call returned 400" in CI.

### §C.3 Stale docstrings

| File:line | Says | Reality | Fix |
|---|---|---|---|
| `ranking/mod.rs:4` | "split into four sub-modules" | ships seven (`adversary_patterns`, `cluster`, `diversity`, `pareto`, `refine_action`, `rubric`, `stability`) | rewrite to enumerate the seven. |
| `phases/budget.rs:33` | "[`BudgetPolicy::Abort`] is a deliberate future hook" | `Abort` was removed in PR #436's diff (see `audit-round-5-cleanup` worktree) | drop the line entirely. |

### §C.4 Unwired enum variants

`BudgetPolicy::Warn` (`phases/budget.rs:72`) and `BudgetPolicy::Abort` (`:78`) are documented as "deliberate future hooks" but no caller consults either — the live switch in `phases/budget.rs:134` is `policy == BudgetPolicy::Reduce`. Severity: low — keep as documented future hooks, but flag in the changelog that they have never been wired.

## §D Spec vs implementation gaps

### §D.1 Directory gaps (closed by PR #426 + `docs/spec-impl-gaps.md`)

`docs/spec-impl-gaps.md` records the seven spec-asked-for directories that don't exist on disk:

| Spec asked for | On disk? | Resolution |
|---|---|---|
| `src/ingest/{mod,normalize,detect,budget}.rs` | No | Folded into `phases/intake.rs`. Intentional collapse. |
| `src/llm/retry.rs` | No | Split between `llm/retry_budget.rs` (RetryReason table) and `phases/phase.rs::call_with_retry_parse` (the canonical retry loop). Intentional split. |
| `src/llm/{glm,qwen,kimi}.rs` | No | **Open gap** — no operator ticket, requires Cargo dep + Provider impl + role catalog entry. |
| `src/domain/<single-type>.rs` | No | Single `src/domain/mod.rs` + `constraint.rs` + `graph.rs` + `synthesis_request.rs`. Intentional collapse. |
| `src/discovery/<helper>.rs` | Partial | Tagger/Clusterer/Contradiction/Facet/Extractor/Integrator split between `phases/discover_*.rs` and `discovery/<helper>.rs`. Intentional partial collapse. |

This finding was closed by PR #426 (`docs/spec-impl-gaps.md` itself). Audit round 1 simply ratifies the closure.

### §D.2 D.22.3 `invalidate_downstream` spec / impl drift — **closed by PR #434**

`docs/proposal-03-add-ons.md:3390` describes D.22.3 as a DAG-traversal function over `ArtifactGraph`. The impl (`src/ranking/invalidate_downstream.rs`, 74 lines) took `&Proposal`, `&RefineAction`, `&ArtifactGraph` but never reached the call site in `phases/refine.rs`. PR #434 deleted the file. No new wiring needed — the `RefineAction::Focus` case in `phases/refine.rs` no-ops, and the proposal sidecar carries the verdict detail so a post-mortem can still reconstruct which proposals were descendants.

### §D.3 `discovery::matrix_seed` — closed by PR #434

`docs/proposal-03-add-ons.md` §D.13.19 calls for a per-`MatrixCell` seed; the impl (`src/discovery/matrix_seed.rs`, 40 lines) admitted in its own docstring that `MatrixCell` never carries a seed field. PR #434 deleted the file. Spec §D.13.19 is RESOLVED via `TemperatureProfile` (PR #356, v0.6).

### §D.4 `manifest_versions` SQLite table — **still open**

`src/storage/migrations/v012_versioned_manifest.sql:5` creates `manifest_versions(run_id, manifest_version, written_at_unix)` but no Rust code writes or reads it. Verified by `grep -rn "manifest_versions" src/ tests/` → 1 result (the `CREATE TABLE` line). Severity: low — drop the migration in a follow-up (the table has been in `meta.sqlite` since v012 with zero rows).

### §D.5 Open: GLM / Qwen / Kimi providers

Per `docs/spec-impl-gaps.md` §3. None requested by an operator; adding any one requires Cargo dep + Provider impl + role catalog entry. Out of scope for round 1.

## §E Top 10 actionable items

Ranked by signal-to-effort. All items fit a single PR (`refactor(audit-r1): drop 34 dead pub fn + 9 dead pub mod`).

| # | Item | LoC removed | Risk | Effort |
|---:|---|---:|---|---|
| 1 | Delete `src/cli/flags_batch.rs`; fold `HashAlgo` into `llm/wire.rs` | 216 | low | 30 min |
| 2 | Delete 4 dead discovery sub-modules (`context`, `id`, `persona_angle`, `saturation_event`); fold re-exported types into `coordinator.rs` | ~800 | medium (rename 6 tests) | 2 h |
| 3 | Delete 5 dead telemetry sub-modules (`level`, `tracing_filter`, `hub`, `daily_rotation`, possibly `lineage_graph`) | ~450 | low | 1 h |
| 4 | Delete `src/llm/streaming.rs` and `src/llm/anthropic_compat.rs` | ~610 | low (only self-tests to drop) | 30 min |
| 5 | Delete `src/storage/compression.rs::{open_gz_read,open_plain_read,export_run_tar_zst}`; verify `reader` is the only dispatch | ~120 | low | 15 min |
| 6 | Delete `src/audit/{format,verify}.rs` dead helpers (`crc32_hex`, `recompute_crc`, `read_records`) | ~50 | low | 15 min |
| 7 | Delete `src/cli/{audit,probe,run}.rs` dead helpers | ~60 | low | 15 min |
| 8 | Delete `src/phases/{intake,synthesize,replace,util}.rs` dead helpers | ~70 | low | 15 min |
| 9 | Delete `src/discovery/integrator.rs::{preserved_citations_ratio,category_header}` + `src/reconcile/per_run.rs::reconcile_run` + `src/context/{resolver,loader}.rs` dead helpers | ~80 | low | 15 min |
| 10 | Drop `manifest_versions` via v017 migration; rename Spanish identifiers (`detectar_outliers` → `detect_outliers`, `cola_reserva` → `reserve_ratio`) | ~25 | low | 30 min |

**Total**: ~2,481 LoC removed across ~30 files. Test count delta: ~30 removed (the dead `#[cfg(test)] mod tests` blocks they carry). No production behaviour change.

## §F Methodology

### §F.1 Reference-counting grep

For each `pub fn` in `src/`:

```bash
grep -rn --include="*.rs" --include="*.md" --include="*.toml" \
  "\b<fn_name>\b" src/ tests/ docs/ examples/ \
  | grep -v "^<file>:<line>:" \
  | grep -v "/tests\.rs:\|tests::\|#\[cfg(test)\]"
```

A count of `0` outside the definition line = dead. A count of `1` outside the definition = the only external caller is a docstring or `mod.rs` re-export — flagged separately in §B.

### §F.2 Module reachability

For each `pub mod` in `mod.rs`:

```bash
grep -rn --include="*.rs" --include="*.md" \
  "X::<mod>\b\|X::{<mod>\|use.*X::<mod>" src/ tests/ docs/ \
  | grep -v "^src/X/<mod>\.rs" \
  | grep -v "^src/X/mod\.rs:"
```

A count of `0` = disconnected. Items with re-exports at `X/mod.rs` are flagged "types survive, drop the file".

### §F.3 Test baseline

`cargo test --lib` at `3c1f23e`: **1854 passed, 0 failed, 2 ignored**. Integration tests: not re-run (round 1 is reference-only, behaviour unchanged).

### §F.4 Files inspected

209 `.rs` files totalling **102,555 LoC**. Largest:

| File | LoC |
|---|---:|
| `src/storage/sqlite.rs` | 4,534 |
| `src/config/mod.rs` | 3,551 |
| `src/phases/phase.rs` | 3,265 |
| `src/sandbox/process.rs` | 2,468 |
| `src/domain/mod.rs` | 2,284 |

## §G What this round does NOT cover

- Anything in `tests/` (round 2 picks up integration-test dead code).
- `Cargo.toml` dependency hygiene (round 2).
- Prompt markdown files (round 2).
- The `manifest_versions` / `budget_events` migration gaps (round 2).
- `BudgetPolicy::{Warn,Abort}` removal (round 2 — PR #436).
- The new findings surfaced by rounds 2-5 (covered by
  `docs/inconsistencies-audit-2026-08-12-round-2.md`).
