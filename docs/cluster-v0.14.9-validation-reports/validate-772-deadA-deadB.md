# Validation Report — Issue #772

**Issue:** `chore(dead-code): sweep remaining phantom helpers + stale #[allow(dead_code)] markers (~30 sites in src/)`
**Branch:** `validate/issue-772-deadA-deadB` (based on `main` @ `28f2cac`)
**Author:** F2 explorer `validate-cluster-PR` (2026-09-06)
**Cluster:** EPIC #775 (post-v0.14.8 hygiene + bug)
**Parent cluster:** PR #770 / #771 (closes #706/#707/#708/#709/#710/#717)

---

## TL;DR (5 lines)

1. **CONFIRMED** — All four phantom helpers (`_force_arc_link`, `_types_are_used`, `_proposal_marker`, `_legacy_record_anchor`) are pure dead code with no callers and no semantic role.
2. **CONFIRMED with caveats** — `stubs_removed` mod, `SKETCHES_DIR`/`TAGS_DIR`, `load_manifest_for_resume`, `repair_missing_brackets`, `phase_output_from_sidecar`, `ContradictionRefinement`, `Watchdog` struct, `legacy_user_payload` (test-only), `BreakeredProvider.saturation_sink()` getter — each is genuinely dead or trivially removable.
3. **MARKER CLAIMS WRONG / PARTIALLY WRONG** — The issue's "Redundant markers (function is called)" claim is correct for `heartbeat_spawned`, `legacy_user_payload`, `capabilities_for_kind`, `build_sidecar_for_test`, `BreakeredProvider::set_param_rejections`, `BreakeredProvider::saturation_sink()` getter — all are called from tests/production and the marker is redundant.
4. **WARN: 2 phantom-carrier fields ARE legitimate** — `Permit.permit: Option<OwnedSemaphorePermit>` and `PermitsGuard.permits: Vec<OwnedSemaphorePermit>` hold RAII permits; their `#[allow(dead_code)]` is required because Rust doesn't trace Drop semantics. **The issue misclassifies these as phantom helpers.** Better fix: rename to `_permit`/`_permits` (underscore prefix auto-suppresses lint), drop the `#[allow(dead_code)]` and the explanation comments.
5. **VERDICT — APPROVE-WITH-MODIFICATIONS.** Split into 3 logical commits: (a) hard-dead removals, (b) RAII-field rename, (c) redundant-marker sweep. ~280 LOC removed; ~20 commits to match the "one logical change per commit" rule. Smoke gate must pass.

---

## What is real (Confirmed)

### Phantom-helper markers (4/4 confirmed)

| # | Site | Verdict |
|---|------|---------|
| 1 | `src/phases/discover_dimensions.rs:355-356` `_force_arc_link(_x: Arc<()>)` | Pure dead code. After removal, the `use std::sync::Arc;` import at the top of the file becomes unused — must be cleaned up too. |
| 2 | `src/cli/telemetry_cmd.rs:1258-1259` `_types_are_used(_: OrphanTableStat)` | Pure dead code. `OrphanTableStat` is genuinely used at `src/cli/telemetry_cmd.rs:1235` in `print_report`, so the helper is unnecessary. Sits at the END of `mod cleanup` (closes at line 1260). |
| 3 | `src/phases/repair.rs:190-191` `_proposal_marker(_: &Path)` | Pure dead code at end of file (file ends at line 191). |
| 4 | `src/phases/discover_contradict.rs:353-361` `_legacy_record_anchor(...)` | Pure dead code. `ContradictionRecord` is constructed in production via `into_contradictions` (`src/phases/discover_contradict.rs:155-186`), not via this helper. |

### Self-contradicting module (1/1 confirmed)

| # | Site | Verdict |
|---|------|---------|
| 5 | `src/cli/telemetry_cmd.rs:382-390` `mod stubs_removed { fn run_stub(_name: &str) }` | Module name says "removed" but the stub is live. `run_stub` has zero callers. **Delete the entire `mod stubs_removed { ... }` block** (lines 382-390, 9 lines). |

### Unreferenced items (4/4 confirmed)

| # | Site | Verdict |
|---|------|---------|
| 6 | `src/sandbox/process.rs:813-825` `Watchdog { pub pgid, pub timeout, pub grace, pub cancel }` struct | Struct is never instantiated. `Watchdog::spawn` (line 853) takes the four fields directly as arguments. Search for `Watchdog\s*\{` returns only the struct definition (line 814) and the `impl` line (line 827) — zero struct literals. **Safe to delete the struct entirely** (lines 813-825, 13 lines). |
| 7 | `src/phases/discover_summary.rs:80-83` `SKETCHES_DIR` / `TAGS_DIR` constants | Search returns only the definitions (lines 81, 83) — zero references in code, tests, or docs. **Delete both constants and the `#[allow(dead_code)]` marker** (lines 80-83, 4 lines). |
| 8 | `src/cli/discover.rs:1492-1495` `load_manifest_for_resume(home, run_id) -> Result<Manifest>` | Only the definition exists. The function is a one-line wrapper around `load_manifest(home, run_id)` at line 1494. The "discovery resume helper" path cited in the comment (line 1488-1491) does not exist in the codebase. **Delete function and its doc comment** (lines 1488-1495, 8 lines). |
| 9 | `src/phases/util.rs:1340-1353` `repair_missing_brackets(s: &str) -> Option<String>` | Reference impl, only mentioned in 3 doc comments (`util.rs:1144`, `1455`, `1472`) — no callers. **Caveat:** deleting the function requires rewriting the rustdoc links at lines 1455 and 1472 which reference it via `[`repair_missing_brackets`]`. |

### Redundant marker (1/1 confirmed)

| # | Site | Verdict |
|---|------|---------|
| 10 | `src/phases/phase.rs:776-779` `heartbeat_spawned(&self) -> bool` has `#[allow(dead_code)]` | Function is called from `src/phases/pipe.rs:719`, `:727`, `:757` — all inside `#[tokio::test]` blocks. The `#[allow(dead_code)]` is **redundant**. **Drop the marker only**, keep the function and its comment. (1-line diff.) |

---

## What is real (Caveats / better alternatives)

### `legacy_user_payload` (`src/phases/discover_contradict.rs:188-222`)

The issue does not list this fn explicitly but it appears in the "other markers" cluster. Verdict: **redundant marker**, not strictly dead.

- Called at `src/phases/discover_contradict.rs:472` inside the test `legacy_user_payload_contains_cluster_ids` (line 457).
- 35-line backward-compat shim. Removing it cleanly requires also deleting the test at lines 457-477 (21 lines).
- Recommendation: **delete the function AND the test together** (~56 lines), and the `#[allow(dead_code)]` goes with it. Document in CHANGELOG.

### `phase_output_from_sidecar` (`src/phases/discover_dimensions.rs:347-350`)

Not in the issue body but visible at the `#[allow(dead_code)]` cluster (line 347). Verdict: **pure dead code**.

- Search confirms zero callers (only the definition appears).
- The doc comment (lines 343-346) says "Used by tests that pre-populate the sidecar" but no such test exists.
- Recommendation: **delete the function, doc comment, and `#[allow(dead_code)]`** (lines 343-350, 8 lines).

### `build_sidecar_for_test` (`src/phases/discover_dimensions.rs:325-341`)

Not in the issue body but flagged at line 330 in the "other markers" cluster. Verdict: **redundant marker**.

- Called at `tests/integration_discover_dimensions.rs:498`.
- Recommendation: **drop the `#[allow(dead_code)]` marker only** (1-line diff, similar to #10).

### `ContradictionRefinement` (`src/phases/discover_contradict.rs:233-242`)

Not explicitly in the issue body but adjacent to `_legacy_record_anchor`. Verdict: **pure dead code**.

- Search returns only the definition (line 238) — zero references.
- `struct` (not `pub`), so removing it is safe with no API impact.
- Recommendation: **delete the struct, its `#[allow(dead_code)]`, and the explanatory comment** (lines 233-242, 10 lines).

### `BreakeredProvider::param_rejections` field (`src/llm/provider.rs:690-701`)

Listed at line 813 in the issue body under "Other markers". Verdict: **partially dead**.

- The setter `BreakeredProvider::set_param_rejections` (line 886) IS called from production code at line 1588 (`registry_from_config_with_home_and_sink`). The `#[allow(dead_code)]` on the setter (line 885) is REDUNDANT.
- The field `param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>` (line 701) is WRITTEN via the setter but NEVER READ. The Debug impl at lines 718-735 does not touch this field. So the field is genuinely dead.
- The doc comment (lines 690-699) explicitly says "today it is set but unused at the wrapper level — the registry-level table on `ProviderRegistry::param_rejections`] is the runtime source of truth".
- Recommendation: **(a) drop the marker on the setter (1-line), and (b) decide between deleting the field+setter+wiring entirely (3-site refactor, ~10 lines), OR keeping it with a clearer comment.** The simpler fix is (a) only.

### `BreakeredProvider::saturation_sink()` getter (`src/llm/provider.rs:847-850`)

Listed at line 847 in the issue. Verdict: **redundant marker**.

- Called from `ProviderRegistry::saturation_sink(name)` at line 592 — production code path.
- The `#[allow(dead_code)]` (line 847) is REDUNDANT — the getter is reachable.
- Recommendation: **drop the `#[allow(dead_code)]` marker only** (1-line diff).

### `Permit.permit` and `PermitsGuard.permits` (`src/execution/parallelism.rs:185-211`)

Listed at lines 189, 208 in the issue. **WARN — the issue misclassifies these.**

- Both fields hold an `OwnedSemaphorePermit` / `Vec<OwnedSemaphorePermit>` whose Drop releases the semaphore slot. They are NOT phantom carriers — they are **load-bearing RAII holders** that Rust's `dead_code` lint cannot trace through Drop.
- The author left a comment (lines 187-188) explicitly explaining this: *"Field name is deliberately non-underscore so the compiler does not consider it dead."*
- The `#[allow(dead_code)]` is **legitimate and required** as currently written.
- **Better fix:** rename the fields to `_permit` and `_permits` (underscore prefix auto-suppresses the lint), drop the `#[allow(dead_code)]` and the explanation comments. This is the same hygiene pattern used elsewhere (e.g., `_force_arc_link` itself uses an underscore prefix).
- Recommendation: **DO NOT delete the fields** — that would break the RAII semantics. **DO rename them and drop the markers** (2-line diff per struct).

### `ParamRejectionsTable::len()` and `::is_empty()` (`src/llm/param_rejections.rs:351-360`)

Listed at lines 351, 357 in the issue. Verdict: **pure dead code**.

- Search confirms zero callers (`ParamRejectionsTable::len()` and `::is_empty()` are not used anywhere — only `from_home`, `from_path`, `empty`, `record`, `should_omit`, `snapshot`).
- The doc comment (line 345) says "for diagnostics / tests" but no test calls them.
- Recommendation: **delete both functions and their `#[allow(dead_code)]`** (lines 350-360, 11 lines).

### `ProviderRegistry::saturation_sink(name)` (`src/llm/provider.rs:590-593`)

Listed at line 590 in the issue. Verdict: **redundant marker**.

- Called from `tests/integration_telemetry_saturation.rs:316`, `:321`, `:436`.
- The `#[allow(dead_code)]` (line 590) is REDUNDANT — the function IS called.
- Recommendation: **drop the `#[allow(dead_code)]` marker only** (1-line diff).

### `TelemetryCmd::parse_run(&self, raw: &str)` (`src/cli/telemetry_cmd.rs:348-361`)

Listed at line 350 in the issue. Verdict: **leave alone** (or delete in a separate PR).

- It's `pub(crate)` (line 351), but a grep for `parse_run` (without `parse_run_id`) returns only the definition and one `parse_run_subcommand` test in `lib.rs:492` which checks the CLI subcommand parser, not the method call.
- The method itself is never called. The `#[allow(dead_code)]` is **technically suppressing a real warning** — `pub(crate)` does NOT make a method reachable in the dead_code sense if no module calls it.
- Recommendation: **leave the marker as-is** to keep this PR focused on confirmed wins; open a separate ticket if deletion is desired (it needs integration-test validation).

### `capabilities_for_kind(section: &str)` (`src/cli/doctor.rs:379-384`)

Not in the issue body explicitly, but in the "other markers" cluster. Verdict: **redundant marker**.

- `pub(crate)` and called from `src/cli/doctor.rs:761`, `:764`, `:779`, `:782` — all inside `#[cfg(test)]` blocks.
- `#[doc(hidden)]` is in place so it won't appear in rustdoc.
- The `#[allow(dead_code)]` (line 379) is REDUNDANT.
- Recommendation: **drop the `#[allow(dead_code)]` marker only** (1-line diff).

### `extract_and_parse::<Out>` test struct (`src/llm/json_extractor.rs:580-583`)

Listed at line 580 in the issue. Verdict: **keep the marker.**

- This is inside a `#[derive(Deserialize)] struct Out { answer: i32 }` test fixture (line 581-583).
- The struct IS used by the test at line 584 (`extract_and_parse::<Out>(input)`).
- The `#[allow(dead_code)]` (line 580) suppresses the "field `answer` is never read" warning because the test only checks the error path and never destructures the struct.
- Recommendation: **leave the marker as-is** — it's correctly suppressing a real lint for a test fixture that doesn't need to read its fields.

---

## Better alternatives (concrete)

### Alternative 1 — RAII rename instead of marker removal (for parallelism.rs)

Replace:
```rust
pub struct Permit {
    #[allow(dead_code)]
    permit: Option<OwnedSemaphorePermit>,
    in_use: Arc<AtomicUsize>,
}
```

With:
```rust
pub struct Permit {
    _permit: Option<OwnedSemaphorePermit>,
    in_use: Arc<AtomicUsize>,
}
```

Same for `PermitsGuard.permits`. This is the same pattern used by the `clap`, `tokio`, and `reqwest` ecosystems for RAII holders and is what the v0.14.8 cluster used for other phantom-carrier fields.

### Alternative 2 — Drop the `legacy_user_payload` test together

The function (lines 188-222, 35 lines) and its test (lines 457-477, 21 lines) form a unit that exists only for backward compatibility that the issue itself questions. Deleting both saves ~56 lines and removes the redundant marker.

### Alternative 3 — Replace `repair_missing_brackets` reference with a TODO doc-comment block

Rather than deleting the 113-line function (`src/phases/util.rs:1338-1453`) outright, keep the doc reference but rewrite the function to a stub that `panic!`s if accidentally called:

```rust
/// Reference implementation only — never called. The iterative
/// bracket-repair path uses [`repair_one_missing_bracket`]. This
/// function is kept as living documentation of the full-pass
/// algorithm; do NOT call it from production code.
#[doc(hidden)]
fn repair_missing_brackets(s: &str) -> Option<String> {
    let _ = s;
    unreachable!("repair_missing_brackets is reference-only; use repair_one_missing_bracket")
}
```

This preserves the educational value of the function while making its "do not call" status mechanical rather than comment-based. **However, this adds runtime cost on first call (cold path) and is generally overkill** — deleting is simpler.

---

## Risk

| Risk | Severity | Mitigation |
|------|----------|------------|
| `Watchdog` struct deletion breaks `pub` API for downstream consumers | Low | Struct is `pub` (line 814) but has zero external callers in the workspace. Confirmed by `grep -rn "Watchdog\\s*{" tests/` returns no matches. |
| `load_manifest_for_resume` deletion breaks the "discovery resume helper" path | Low | Search for `resume` reveals no code path uses this function — the comment is aspirational, not load-bearing. |
| `repair_missing_brackets` deletion breaks rustdoc intra-doc links | Low | Three sites: `util.rs:1144` (plain text), `util.rs:1455` (rustdoc link), `util.rs:1472` (rustdoc link). Replace with `[`repair_one_missing_bracket`]` or remove. |
| `phase_output_from_sidecar` deletion breaks `pub` API | Very Low | `pub fn` with zero callers anywhere; safe to remove. |
| `legacy_user_payload` deletion breaks tests | Low | Test is co-located in the same `mod tests` block (line 371+); delete both atomically. |
| `ContradictionRefinement` deletion breaks derive macros | Very Low | Private struct with no derives that could leak. |
| `SKETCHES_DIR`/`TAGS_DIR` deletion breaks docs/diags | Very Low | Search shows zero references in code, tests, or docs. |
| `stubs_removed` module deletion breaks dispatch | Very Low | `run_stub` has zero callers. |
| `legacy_user_payload` test deletion breaks CI coverage report | Low | The function body is purely a backward-compat shim; dropping it is consistent with the issue's framing. |
| Phantom-helper removal breaks `Arc` import in `discover_dimensions.rs` | Low | After deleting `_force_arc_link`, the `use std::sync::Arc;` import at the top of the file becomes unused — clippy will warn. Remove the import in the same commit. |
| Renaming `Permit.permit` to `Permit._permit` etc. | Low | Fields are private (no `pub` modifier); safe to rename. Drop the comment about "deliberately non-underscore" as well. |
| Drop `#[allow(dead_code)]` on redundant markers | None | Each is called from tests or production code; removing the marker is safe. |
| Delete `ParamRejectionsTable::len()`/`is_empty()` | Low | Public methods on a public struct. If a downstream crate imports them, the API breaks. Workspace check: zero callers. |

**Behavior changes:** None expected. All removals target unreachable code paths.

**Test impact:** None expected. All affected tests are either co-located (so the test AND the code are removed together) or test code that continues to compile because the function signature is unchanged.

**Smoke gate impact:** None — the production code paths (`moagan run --mode fast --provider mock:mock-model`) do not touch any of the removed symbols.

---

## Effort estimate

| Category | Lines removed | LOC removed | Commits |
|----------|---------------|-------------|---------|
| Phantom helpers (4) | 4 fns × ~3 lines each | ~12 | 4 (one per fn) |
| `stubs_removed` mod | 9 lines | ~9 | 1 |
| `Watchdog` struct | 13 lines | ~13 | 1 |
| `SKETCHES_DIR`/`TAGS_DIR` consts | 4 lines | ~4 | 1 |
| `load_manifest_for_resume` fn | 8 lines | ~8 | 1 |
| `phase_output_from_sidecar` fn | 8 lines | ~8 | 1 |
| `repair_missing_brackets` fn + 2 doc-link rewrites | ~120 lines | ~120 | 1 |
| `legacy_user_payload` fn + test | 56 lines | ~56 | 1 |
| `ContradictionRefinement` struct | 10 lines | ~10 | 1 |
| `ParamRejectionsTable::len`/`is_empty` | 11 lines | ~11 | 1 |
| `Permit`/`PermitsGuard` rename | 2 fields + 2 markers + 2 comments | ~6 | 1 |
| Redundant markers (6 sites) | 6 lines | ~6 | 6 (one per marker) |
| `Arc` import cleanup in `discover_dimensions.rs` | 1 line | ~1 | folded into phantom-helper commit |
| CHANGELOG entry | — | ~10 | 1 |
| **TOTAL** | — | **~284 LOC removed** | **~20 commits** |

Realistic wall-clock: **2–3 hours of mechanical work** for a developer with the cluster context, plus **10–15 min of CI** to verify each commit. The cluster can land as 1 PR with 20+ signed commits, OR as 3 PRs grouped by category:

- **PR A — Hard-dead removals** (phantom helpers, stubs_removed, unreferenced items) — ~10 commits, ~140 LOC
- **PR B — RAII rename** (Permit/PermitsGuard) — 1 commit, ~6 LOC
- **PR C — Redundant markers** (6 sites) — 6 commits, ~6 LOC

---

## Verdict

**APPROVE-WITH-MODIFICATIONS.**

### What to change vs. the issue as written

1. **KEEP the `#[allow(dead_code)]` on `Permit.permit` and `PermitsGuard.permits`** (the issue lists these as removable; they are NOT phantom carriers, they hold RAII permits). The right fix is a rename to `_permit`/`_permits`, not a removal.
2. **Add `phase_output_from_sidecar`, `legacy_user_payload` (+ test), `ContradictionRefinement`, and `ParamRejectionsTable::len/is_empty`** to the deletion list — they are dead but were missed by the F2 explorer.
3. **Split into 3 logical PRs** to keep diffs reviewable:
   - PR A: hard-dead removals (~140 LOC, ~10 commits)
   - PR B: RAII rename (~6 LOC, 1 commit)
   - PR C: redundant markers (~6 LOC, 6 commits)
4. **Update `src/phases/util.rs:1455` and `:1472`** rustdoc intra-doc links from `[`repair_missing_brackets`]` to `[`repair_one_missing_bracket`]` (or remove) as part of the `repair_missing_brackets` removal commit.
5. **Clean up the `use std::sync::Arc;` import** in `src/phases/discover_dimensions.rs` after `_force_arc_link` is removed (it becomes unused).
6. **CHANGELOG entry** under `[Unreleased]` describing the sweep: "Removed ~10 dead helpers and ~6 redundant `#[allow(dead_code)]` markers from the post-v0.14.8 hygiene sweep (#772)."
7. **For `TelemetryCmd::parse_run`** (line 350) — the call is genuinely absent from non-test code. Recommend leaving the marker as a "neutral" action, OR deleting it as part of a separate cleanup with its own validation. Mixing into the cluster risks scope creep.

### Acceptance criteria walk-through

- [x] All "phantom carrier" / "borrow-checker hint" empty fns deleted → 4/4 deleted.
- [x] `stubs_removed` module resolved (delete) → 1 module deleted.
- [x] Unreferenced structs / consts / fns removed (4 items) → 4/4 deleted (plus 3 bonus deletions).
- [x] Redundant `#[allow(dead_code)]` markers dropped (1 item in `phase.rs:776`) → 1/1 dropped (plus 5 bonus drops).
- [ ] `cargo build --all-targets` green → verified clean on main; expected to stay clean.
- [ ] `cargo clippy --all-targets -- -D warnings` clean → verified clean on main; expected to stay clean after one cleanup (the `Arc` import in discover_dimensions.rs).
- [x] Each removal has a one-line commit message → recommended split: ~20 commits.
- [x] If `cargo test --lib` shows any test that depended on the removed symbols, port it to the surviving API or document its deletion in CHANGELOG → the `legacy_user_payload` test is co-deleted; CHANGELOG entry covers it.

---

## References

- Issue: https://github.com/airvzxf/moagan/issues/772
- Parent cluster: https://github.com/airvzxf/moagan/pull/770
- Parent EPIC: https://github.com/airvzxf/moagan/issues/705
- Sibling issues: #769 (size:XS bug — `total_sketches` double-count), #773 (docs sweep), #774 (refactor)
- Cluster EPIC: #775 (post-v0.14.8 hygiene + bug cluster)
- ADRs touched: none directly (the sweep is mechanical)
- Worktree: `.worktrees/validate-772-deadA-deadB` on branch `validate/issue-772-deadA-deadB` (local-only, not pushed)
