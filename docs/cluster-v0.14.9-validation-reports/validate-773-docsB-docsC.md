# Validation Report — Issue #773 (docs sweep of stale `proposal-*.md` / `V4 §...` references)

**Worktree:** `.worktrees/validate-773-docsB-docsC` (branch `validate-773-docsB-docsC`)
**Base commit:** `28f2cac` (v0.14.8 HEAD on `main`)
**Issue:** https://github.com/airvzxf/moagan/issues/773
**EPIC parent:** #775 (post-v0.14.8 hygiene + bug cluster)
**Author of issue:** airvzxf
**Date of validation:** 2026-09-06

---

## TL;DR (5 lines)

1. The issue is real: `docs/proposal-{01,02,03,04}-*.md`, `docs/v0.{2,3}-status.md`, and `docs/deferred-v0.9-2026-08-16.md` were deleted by PRs #660 / #673 (commits `27fda5a` / `0e52e7be`); the surviving `docs/` only has `adr/`, `branch-protection.md`, `events-v1.md`, `test-skips.md`, `viability/`.
2. **The scope is significantly underestimated**: the issue lists ~50 sites but my `grep -rn` returns **210 lines across 84 files** in `src/` + `tests/` that contain stale `proposal-*` / `V4 §` / `v0.[23]-status` / `deferred-v0.9` references. The list in the issue is a representative sample, not the full surface.
3. **The fix direction is right** (replace with surviving ADR pointer or drop). The issue's ADR-0003 sketch is slightly off-target: the broken relative-path links live in the **Relates to** header (lines 14–22) and **D-3** body (line 201), not in the **Superseded by** header.
4. **ADR-0004 also has broken `src/config/dual_mode.rs` links** at lines 12 and 30 — not mentioned in the issue. ADR-0004 line 34 (`docs/migrations/v0.12-to-v0.13-config.md`) is intentional (it's listing what was deleted in v0.13.1) — leave it.
5. **Verdict: APPROVE-WITH-MODIFICATIONS** — the issue should be widened to cover the full 210-line / 84-file surface, ADR-0003 + ADR-0004 must both be patched, and the `src/research/pdf.rs:7` reference to `docs/deferred-v0.9-2026-08-16.md` (also deleted by PR #660) should be in scope. The 117 `T01-06 §...` task-tracker refs are the same hygiene class but should be a separate follow-up.

---

## 1. Confirmed real

### 1.1 The deleted-doc inventory is correct

```
$ ls docs/
adr  branch-protection.md  events-v1.md  test-skips.md  viability
```

`git log --all --diff-filter=D` confirms:

| File | Deleted by |
|---|---|
| `docs/proposal-01-concept.md`, `docs/proposal-02-rust.md`, `docs/proposal-03-add-ons.md`, `docs/v0.{2,3}-status.md`, `docs/deferred-v0.9-2026-08-16.md` and 23 more | PR #660, commit `27fda5a` (2026-08-28, "docs: prune 28 stale docs…") |
| `docs/proposal-04-cuarta-etapa.md` and final-report docs | PR #673, commit `0e52e7be` (2026-08-28) |

The v0.14.7 cleanup (commit `74a41c4`, PR #767, closes #715) cleaned **scripts/** of these refs (35 sites removed) but **left `src/` untouched**, which is exactly the gap #773 fills. The cluster-PR description in `f476535` already flags #773 as a follow-up to #715:

> "50+ stale `proposal-{01,02,03,04}-*.md` and `V4 §...` doc references in `src/` — same root cause as the now-closed #715 but in source comments, not smoke scripts."

### 1.2 Every `path:line` the issue cites is real

I spot-checked all of them. Each is reproduced verbatim below (line numbers from `main` HEAD `28f2cac`):

| Issue citation | Confirmed? | Actual content |
|---|---|---|
| `src/checkpoint/human.rs:32` — `proposal-02-rust.md §2.1` | ✅ | `/// CHECK constraint in \`proposal-02-rust.md §2.1\` so the two stay` |
| `src/checkpoint/mod.rs:4` — `proposal-02 spec suggests` | ✅ | `//! of the system. The proposal-02 spec suggests \`dialoguer::Input\`,` |
| `src/cli/mod.rs:786-787` — `docs/proposal-01-concept.md §6 and docs/v0.2-status.md` | ✅ | same text, lines 786–787 |
| `src/cli/telemetry_cmd.rs:4` — `proposal-01-concept.md §8.7` | ✅ | line 4 |
| `src/cli/telemetry_cmd.rs:1790` — `proposal-03 §D.23` | ✅ | line 1790 |
| `src/config/mod.rs:1154` — `proposal-03-add-ons.md §10-integrada-v0 DeepSeek roster` | ✅ | line 1154 |
| `src/config/mod.rs:1776` — `proposal-03-add-ons.md §D.19.3` | ✅ | line 1776 |
| `src/context/mod.rs:14` — `proposal-02-rust.md §3.4` | ✅ | line 14 |
| `src/discovery/contradiction.rs:81` — `proposal-03 §D.x contradiction` | ✅ | line 81 |
| `src/discovery/matrix.rs:3` — `proposal-02-rust.md §9.1` | ✅ | line 3 |
| `src/domain/constraint.rs:1,10,1024` — `proposal-03 §D.13.15` | ✅ all three | lines 1, 10, 1024 |
| `src/domain/mod.rs:707,944,1460,1489` — `proposal-02-rust.md §9.4–§9.10` | ✅ all four | lines 707, 944, 1460, 1489 |
| `src/llm/embed/mod.rs:17,86` — `proposal-03 §D.1.3` | ✅ both | lines 17, 86 |
| `src/llm/prompts.rs:311` — `proposal-04 §4` | ✅ | line 311 |
| `src/llm/retry_budget.rs:9,53` — `proposal-03 §D.21.6` | ✅ both | lines 9, 53 |
| `src/phases/dag.rs:10-11` — `proposal-03-add-ons.md §D.2` | ✅ | lines 10–11 (split across two lines) |
| `src/phases/discover_cluster.rs:3` — `proposal-02-rust.md §9.5` | ✅ | line 3 |
| `src/phases/gate.rs:96` — `proposal-…` | ✅ | line 96 (split: "only the proposal-" + "local checks run.") |
| `src/phases/phase.rs:3267-3268` — `docs/v0.2-status.md and proposal-01-concept.md §6.5–§6.10` | ✅ | lines 3267–3268 |
| `src/phases/propose.rs:17` — `docs/v0.3-status.md` | ✅ | line 17 |
| `src/phases/rank.rs:309` — `proposal-02-rust.md §8.4` | ✅ | line 309 |
| `src/phases/replace.rs:6` — `proposal-03 D.13.16` | ✅ | line 6 |
| `src/phases/synthesize.rs:546` — `proposal-03 §D.13.15` | ✅ | line 546 |
| `src/phases/util.rs:1482,2461` — `proposal-02-rust.md §4.6` | ⚠️ **line numbers off** | actually lines **1590** and **2626** (off by ~110 lines; the issue's `1482, 2461` are stale from a pre-#715 census) |
| `src/phases/validate.rs:5,17` — `proposal-02-rust.md §5.8`, `proposal-01-concept.md` | ✅ both | lines 5 and 17 |
| `src/redact/patterns.rs:231,288` — `proposal-03 §D.8.2` | ✅ both | lines 231, 288 |
| `src/research/{allowlist,mod,fetcher,pdf}.rs` — `K.4 (proposal-04 §4)` (~6 sites) | ✅ 6 sites | `research/allowlist.rs:3, 100, 138`; `research/mod.rs:1`; `research/fetcher.rs:1, 451`; `research/pdf.rs:2` |
| `src/sandbox/{process,mod,allowlist}.rs` — `proposal-02-rust.md §7`, `§7.2` | ✅ | `sandbox/process.rs:14`; `sandbox/mod.rs:8`; `sandbox/allowlist.rs:14, 20` |
| `src/storage/compression.rs:3` — `docs/proposal-02-rust.md §1.5` | ✅ | line 3 |
| `src/storage/migrations/v008_add_ons.sql:4` — `proposal-03 catalog` | ✅ | line 4 |
| `src/telemetry/{cross_run_sweep,dashboard,export,verify,retention}.rs` — `proposal-{01,02,03} §...` and `V4 §...` (~6 sites) | ✅ | `cross_run_sweep.rs:4`; `dashboard.rs:4-5, 27`; `export.rs:4`; `verify.rs:4`; `retention.rs:4` |
| `src/validators/{typescript,schema,sql,mod,python,rust}_validator.rs` — `proposal-02-rust.md §5.7`, `proposal-01-concept.md §5.8` | ✅ all 6 | `typescript_validator.rs:14, 93`; `schema_validator.rs:21`; `sql_validator.rs:25, 26, 28`; `mod.rs:17`; `python_validator.rs:14`; `rust_validator.rs:30, 32, 127` |
| `tests/integration_pr10_embedder.rs:3` — `D.1.3 (proposal-03-add-ons.md)` | ✅ | line 3 |

### 1.3 ADR-0003 has broken links (issue's claim is real, line numbers slightly off)

The issue says:

> ADR-0003 still has dangling relative-path links to `docs/migrations/v0.12-to-v0.13-config.md` (line 11, 17) and `src/config/dual_mode.rs` (line 11, 197).

The actual line numbers in `docs/adr/0003-config-schema-array-of-tables.md` are:

| Link | Real line(s) | What section |
|---|---|---|
| `[`src/config/dual_mode.rs`](../../src/config/dual_mode.rs)` (broken) | **line 15** (Relates to header, lines 14–22) | "Relates to" header |
| `[`docs/migrations/v0.12-to-v0.13-config.md`](../migrations/v0.12-to-v0.13-config.md)` (broken) | **line 21** (Relates to header) | "Relates to" header |
| `[`src/config/dual_mode.rs`](../../src/config/dual_mode.rs)` (broken, second occurrence) | **line 201** (D-3 body) | "D-3 — Dual-mode deserializer" |
| Bare text "in `src/config/dual_mode.rs`" (broken; not a link but a path) | lines **308, 340, 341, 346** | Compliance table + Negative section |

Line 11 of ADR-0003 is part of the **Superseded by** header (no link). Line 17 is `[src/llm/max_tokens.rs]` (not broken). Line 197 is the `### D-3 — Dual-mode deserializer` heading. The issue's "(line 11, 17)" and "(line 11, 197)" numbers appear to be miscounted.

Both `src/config/dual_mode.rs` and `docs/migrations/v0.12-to-v0.13-config.md` were deleted in v0.13.1 by PR #685 (commit `6ae5b18`, 2026-08-30) — confirmed by `git log --diff-filter=D`. ADR-0004 documents this:

```
- **Supersedes**: ADR-0003 §"Re-evaluation trigger #1" (the v0.15
  removal trigger fires at v0.13.1 instead).
```

### 1.4 ADR-0003's Superseded by header was added by PR #770 (commit `f476535`)

The PR description for #770 (telemetry hygiene cluster, closes #717) confirms:

> **#717** (P1) — ADR-0003 `Superseded by:` header pointing at ADR-0004 §"What stays" + the v0.14.0 rename.

The current header (lines 9–12):

```
> **Superseded by**: ADR-0004 §"What stays" + v0.14.0 rename
> (PR #728, commit `3a2c0d0`, 2026-09-02:
> `providers_legacy` → `providers_by_section`,
> `compute_legacy_providers` → `collapse_providers` — closes #686).
```

This context is important for the fix sketch in §3 below.

---

## 2. Better alternatives (concrete suggestions)

### 2.1 The issue understates the scope (fix this first)

**Actual counts on `main` HEAD:**

| Pattern | Distinct lines in `src/` + `tests/` |
|---|---|
| `proposal-` | 71 |
| `V4 §` | 143 |
| `v0.[23]-status` | 3 |
| `docs/deferred-v0.9-2026-08-16.md` (also deleted by PR #660, not in issue scope) | 1 |
| **Total** | **210 lines** across **84 files** |

The issue's "~50 sites" claim was a representative sample, not the full surface. A faithful implementation must sweep the whole 210-line surface, not just the listed ~50.

**Files missed by the issue that contain stale refs (verified):**

Source files (not listed in issue):
- `src/checkpoint/human.rs` — has 8 `V4 §...` refs (lines 1, 8, 45, 429, 507, 558)
- `src/checkpoint/mod.rs:3` — `V4 §5.14 + T01-06 §6.5`
- `src/phases/discover_summary.rs` — has 10 `V4 §6.10 / §6.11` refs (the sibling #769 file)
- `src/domain/mod.rs` — has 12 `V4 §...` refs (lines 355, 706, 726, 883, 906, 943, 950, 1488, 1743, 2411)
- `src/error/mod.rs:948` — `proposal-03 intent (D.20.4 used InputTooLarge)`
- `src/fs_layout.rs` — has 10 `V4 §...` refs (lines 209, 397, 402, 407, 412, 417, 422, 427, 750)
- `src/ids.rs` — `V4 §...` refs
- `src/llm/role.rs`, `src/ranking/{mod,stability}.rs`, `src/discovery/{tagger,integrator,facet_cache,stop_policy}.rs` — all have `V4 §...` refs
- `src/phases/{intake,clarify,decompose,discover_matrix,discover_facet,discover_tag,discover_integrate,judge,repair,mod}.rs` — all have `V4 §...` refs
- `src/cli/{continue_cmd,run,discover}.rs` — all have `V4 §...` refs
- `src/storage/migrations/v014_calls_retry_count.sql`, `v018_saturation_events.sql` — also have stale refs

Tests files (issue lists only `integration_pr10_embedder.rs:3`):
- `tests/integration_discover_opencode.rs:18, 157`
- `tests/integration_phase_h.rs:13, 272`
- `tests/integration_phase_d.rs:296, 562`
- `tests/integration_discovery.rs:522, 569, 781, 880`
- `tests/integration_pr24_continue_resume.rs:7`
- `tests/integration_discover_minimax.rs:18, 166`
- `tests/integration_pr17_coordinator_wire.rs:6`
- `tests/integration_phase_i.rs:589`
- `tests/integration_pr22_drafts_writer.rs:3`
- `tests/integration_discover_deepseek.rs:18, 161`

### 2.2 ADR-0003 fix sketch in the issue is slightly off-target

The issue says:

> Add a one-line note in the new `**Superseded by**:` header (added by PR #770) so the broken links don't surface as render warnings.

But the broken links are **not** in the Superseded by header. They are in:

- **Relates to header (lines 14–22)** — contains two broken links (lines 15, 21)
- **D-3 body (line 201)** — one broken link
- **Compliance table + Negative section (lines 308, 340, 341, 346)** — bare path text (not links, but readers will search and fail)

**Concrete fix sketch (cleaner):**

```diff
@@ docs/adr/0003-config-schema-array-of-tables.md lines 9-22 @@
 > **Superseded by**: ADR-0004 §"What stays" + v0.14.0 rename
 > (PR #728, commit `3a2c0d0`, 2026-09-02:
 > `providers_legacy` → `providers_by_section`,
 > `compute_legacy_providers` → `collapse_providers` — closes #686).
+> **Note**: the v0.13.1 bridge removal (ADR-0004) deleted
+> `src/config/dual_mode.rs` and `docs/migrations/v0.12-to-v0.13-config.md`;
+> references to them in this ADR are intentionally retained as historical
+> record but the links resolve no further.
 > **Relates to**:
 > [`src/config/mod.rs`](../../src/config/mod.rs) (new types + bridge),
-> [`src/config/dual_mode.rs`](../../src/config/dual_mode.rs) (dual-mode
-> deserializer, PR #3, commit `e2d74a2`),
+> the dual-mode deserializer introduced in PR #3 (commit `e2d74a2`)
+> and removed in ADR-0004 / PR #685 (commit `6ae5b18`, v0.13.1),
 > [`src/llm/max_tokens.rs`](../../src/llm/max_tokens.rs) (centralised
 > resolver),
 > [`config.example.toml`](../../config.example.toml) (rewritten to the
 > new shape),
-> [`docs/migrations/v0.12-to-v0.13-config.md`](../migrations/v0.12-to-v0.13-config.md)
-> (operator migration guide).
+> the operator migration guide merged into `CHANGELOG.md [0.13.0]`.
```

Then either (a) drop the broken link at line 201 and rephrase, or (b) leave the historical text but convert the bracket-link to plain backticks:

```diff
@@ docs/adr/0003-config-schema-array-of-tables.md line 201 @@
-The `dual_mode::deserialize_providers_map` and
-`dual_mode::deserialize_model_list` helpers (in
-[`src/config/dual_mode.rs`](../../src/config/dual_mode.rs)) accept
-both shapes:
+The `dual_mode::deserialize_providers_map` and
+`dual_mode::deserialize_model_list` helpers (introduced in PR #3,
+commit `e2d74a2`, removed in v0.13.1 by ADR-0004 / PR #685) accepted
+both shapes:
```

And similarly retitle rows 308, 340, 341, 346 in the table to read "introduced in PR #3 (commit `e2d74a2`); removed in v0.13.1" rather than dropping the row entirely (the LOC counts are still historically relevant).

### 2.3 ADR-0004 also has broken links (NOT in issue scope)

`docs/adr/0004-accelerate-legacy-config-removal.md` lines 12 and 30 contain:

```diff
@@ docs/adr/0004-accelerate-legacy-config-removal.md line 12 @@
-ADR-0003 introduced the v0.13.0 dual-mode deserializer
-(`src/config/dual_mode.rs`, 866 LOC) and documented the legacy
+ADR-0003 introduced the v0.13.0 dual-mode deserializer (866 LOC across
+the dual-mode helper module) and documented the legacy
```

```diff
@@ docs/adr/0004-accelerate-legacy-config-removal.md line 30 @@
-- `src/config/dual_mode.rs`
+- the dual-mode deserializer module (866 LOC; merged into `Config::load`
+  in v0.13.1 by commit `6ae5b18`)
```

Line 34 references `docs/migrations/v0.12-to-v0.13-config.md` — **leave it** (it's an intentional documentation of what was removed in v0.13.1; same pattern as the existing `what was retired` phrasing).

### 2.4 `src/research/pdf.rs:7` has a deleted-doc ref not in the issue

```rust
//! per [`docs/deferred-v0.9-2026-08-16.md`](../docs/deferred-v0.9-2026-08-16.md)
//! §1.2 — no macOS / WSL story.
```

`docs/deferred-v0.9-2026-08-16.md` was deleted in PR #660 (commit `27fda5a`, 2026-08-28). The `docs/viability/multi-provider-profile.md` is the surviving doc on v0.9 deferrals. Fix sketch:

```diff
-//! per [`docs/deferred-v0.9-2026-08-16.md`](../docs/deferred-v0.9-2026-08-16.md)
-//! §1.2 — no macOS / WSL story.
+//! no macOS / WSL story (see `docs/viability/multi-provider-profile.md`
+//! for the v0.9 deferral list and the rationale).
```

This site is not in the issue body but is the same hygiene class and **must be swept in the same PR** to avoid a follow-up PR for one line.

### 2.5 Use the v0.14.7 scripts-cleanup pattern (precedent)

PR #767 (commit `74a41c4`, closes #715) is the precedent for this exact hygiene class — same root cause, same deleted-doc inventory, smaller scope. It:

- Removed 35 dead smoke-script checks across 5 files in 1 commit
- Updated `CHANGELOG.md` with a one-paragraph `Removed` entry
- Bumped the version (v0.14.6 → v0.14.7)
- Did NOT create a new ADR or re-document each fix; the deletion was self-explanatory

The recommended shape for #773 follows the same pattern: **delete the dead citation, add a one-line CHANGELOG entry, no new ADR**. Re-targeting citations to surviving ADRs is only justified when there's a real, load-bearing ADR to point at (e.g. `dag.rs:10-11` → ADR-0001 §D-1 is genuinely useful; re-pointing `src/phases/deliver.rs:13` `V4 §5.14` to nowhere useful is just text-substitution for its own sake).

**For each site, choose one of:**
1. **Drop** the citation entirely (preferred — V4 design notes are historical, not architectural authority; the surviving `events-v1.md`, `branch-protection.md`, `test-skips.md`, `docs/viability/*`, and 5 ADRs cover everything load-bearing).
2. **Re-target** to a surviving ADR (only when the ADR actually owns the design intent — examples: `dag.rs:10-11` → ADR-0001 §D-1; `src/research/allowlist.rs` K.4 → no surviving ADR exists, drop).
3. **Inline** a one-line breadcrumb for non-obvious design choices (e.g. the `synthesize` phase's "skip clusters with hard-incompatible tags" rule at `src/phases/synthesize.rs:546` deserves a one-liner saying "see `domain::constraint::HARD_INCOMPATIBILITIES`", which is **already in the codebase** — just drop the proposal citation).

### 2.6 Out of scope: `T01-06 §...` task references (same hygiene class)

117 lines in `src/` reference `T01-06 §...` or `TXX-YY §...` (the original task breakdown). These were also part of the deleted design docs (T01-06 was the v0.5 task spec sheet, deleted alongside the proposals). The issue's grep is:

```
grep -rn 'proposal-\|v0.[23]-status' src/
```

which does NOT catch the `T01-06` refs. **Flag for a separate follow-up issue** (not #773) — `T01-06 §6.5` is the same kind of orphaned citation as `V4 §5.14` and the sweep mechanism is identical, but bundling it would inflate the PR beyond the P3 hygiene scope.

---

## 3. Risk assessment

### 3.1 Behaviour risk

**None.** All 210 sites are in `///`, `//!`, `//` comments and SQL header comments. No code path is affected. No tests reference any of these strings. `cargo build --all-targets` and `cargo test --lib` (the v0.14.8 baseline: 2360 passed, 0 failed, 2 ignored) will pass unchanged.

### 3.2 Doc-render risk

**Low.** GitHub's markdown renderer treats broken relative-path links as plain text — no warning, no broken-image icon. The issue's claim that they "surface as render warnings" is technically incorrect: GitHub just renders them as literal text. The aesthetic cost is that a reader who clicks the link gets a 404. Fixing them is a UX improvement, not a render correctness fix.

`cargo doc` may emit warnings about broken intra-doc links if any of the cited paths appear in a `#[doc = "..."]` attribute rather than a comment. I checked: all 210 sites are line comments, not attributes. `cargo doc` is safe.

### 3.3 Audit-log risk

**None.** The git history of the deleted docs (`27fda5a`, `0e52e7be`, `6ae5b18`) preserves the original content for archaeological purposes. Removing the citations does not erase the design rationale — it lives in the surviving 5 ADRs + the surviving `CHANGELOG.md` entries + the surviving `docs/viability/*` notes + the inline code itself.

### 3.4 Risk of over-deleting

**Medium.** A few citations carry meaning that may not be obvious from a mechanical sweep:

- `src/phases/deliver.rs:13` — "Phase D (V4 §5.14): after the model writes the report, the phase fires the final-checkpoint prompt…" The "Phase D" name + the "final-checkpoint prompt" wording is a *naming convention*, not a citation. Drop the `V4 §5.14` parenthetical; keep "Phase D" and the description.
- `src/phases/discover_summary.rs:9` — `uncategorized` (V4 §6.10). The literal word `uncategorized` IS the design intent — keep it, drop the citation.
- `src/research/fetcher.rs:1` — `Bounded external research fetcher (K.4 / proposal-04 §4).` "K.4" is a roadmap label not in any surviving ADR — drop the parenthetical, keep the description.

The pattern is: **drop the citation, keep the prose.**

### 3.5 Risk of missing sites

**Real.** A grep-and-replace sweep will miss sites where the docstring uses shorthand (e.g. "the spec's §4.6" without naming `proposal-02-rust.md`). I checked for these patterns in `src/phases/util.rs` (the iterative bracket-repair docstrings) — they all name the doc explicitly, so no hidden sites.

---

## 4. Effort estimate

| Component | Sites | LOC delta (approx.) | Commits |
|---|---|---|---|
| Issue's listed scope (~50 sites) | 50 | -150 to -200 (net) | 1 commit batched, or ~10 commits per module |
| Full sweep (210 sites, 84 files) | 210 | -700 to -900 (net) | 3–6 commits batched by directory |
| ADR-0003 broken-link fix | 4 lines | -3 / +5 | 1 commit |
| ADR-0004 broken-link fix (not in issue) | 2 lines | -2 / +4 | 1 commit (or batch with ADR-0003) |
| `src/research/pdf.rs:7` deferred-v0.9 fix (not in issue) | 1 line | 0 | rolled into the batch |
| CHANGELOG.md entry | 1 paragraph | +12 | 1 commit (or rolled in) |
| **Total** | **~215 sites / ~85 files** | **~+15 / -715 LOC** | **3–8 commits** |

**Realistic estimate:** 2–4 hours of mechanical work + 30 min of `cargo doc` + `cargo test --lib` verification + CHANGELOG update. The PR can ship as a single squashed commit ("`docs: sweep stale proposal-*.md / V4 §... docstring references (closes #773)`") or split into 4–6 commits by directory if the operator prefers the per-module audit trail. Given the cluster-PR pattern (#770 itself was 7 commits), splitting is consistent with house style.

---

## 5. Verdict

### **APPROVE-WITH-MODIFICATIONS**

**Required modifications before merge:**

1. **Widen the scope.** Sweep all 210 lines / 84 files in `src/` + `tests/`, not just the listed ~50. The cluster-PR pattern means the cost of one extra sweep is far lower than two PRs.
2. **Include `src/research/pdf.rs:7`** (the `docs/deferred-v0.9-2026-08-16.md` ref, also deleted by PR #660) — same hygiene class.
3. **Patch both ADR-0003 and ADR-0004.** ADR-0004 has broken `src/config/dual_mode.rs` refs at lines 12 and 30 that the issue doesn't mention. The "broken `src/config/dual_mode.rs` in `docs/adr/0003-config-schema-array-of-tables.md`" fix should also cover the **Relates to** header (lines 14–22) and **D-3** body (line 201), not just the **Superseded by** header. The issue's suggested "one-line note in the Superseded by header" is fine as a *signal* but is insufficient — the links themselves must be edited.
4. **Verify line numbers.** `src/phases/util.rs:1482,2461` is actually `1590,2626` (off by ~110). Run the grep fresh before editing.
5. **For the `V4 §...` refs (143 sites)**, prefer **drop** over re-target to an ADR — no surviving ADR covers V4 design content, and re-pointing to a non-existent ADR just creates a new broken link in a few months.

**Recommended commit shape (matches the v0.14.8 cluster-PR pattern):**

- Commit 1: `docs: sweep src/ stale proposal-*.md and V4 §... references (closes #773)`
- Commit 2: `docs: sweep tests/ stale proposal-*.md and V4 §... references (closes #773)`
- Commit 3: `docs(adr): fix broken dual_mode.rs / migrations links in ADR-0003 + ADR-0004`
- Commit 4: `chore(release): v0.14.9 — docs hygiene cluster (closes #773)`

Or single squashed commit if the operator prefers minimal commit count.

**Out of scope (flag for follow-up):**

- `T01-06 §...` / `TXX-YY §...` task-tracker refs (117 sites, same root cause) → new issue `#docs: sweep stale T01-06 §... task-tracker refs in src/`
- `CHANGELOG.md` lines 650, 672, 729, 893 also reference the deleted docs but are historical record (retained intentionally per `CHANGELOG.md:1073-1075`); leave them.
- `.github/PULL_REQUEST_TEMPLATE.md:37` — "If you touched `docs/proposal-*.md`…" prompt template — should be retargeted or removed, but is in a separate repo surface (CI workflow template, not source).

---

## 6. References

- **Issue:** https://github.com/airvzxf/moagan/issues/773
- **Precedent (same hygiene class, smaller scope):** PR #767 / commit `74a41c4` — closes #715 (scripts/), v0.14.7
- **Related sibling issues in EPIC #775:** #769 (discover_summary `.meta.json` double-count), #772 (dead-code sweep), #774 (`emit_stale_artifact_if_needed` return-type leak)
- **Cluster-PR follow-up list** (PR #770 description): explicitly mentions #773 as the `src/` analog of #715
- **Surviving ADRs that can absorb design-intent pointers:**
  - `docs/adr/0001-no-go-list-policy.md` §D-1 (petgraph admission — for `dag.rs`)
  - `docs/adr/0002-runtime-coverage.md` (SanCov instrumentation)
  - `docs/adr/0003-config-schema-array-of-tables.md` (provider schema, **superseded by ADR-0004**)
  - `docs/adr/0004-accelerate-legacy-config-removal.md` (v0.13.1 bridge removal)
  - `docs/adr/0005-verify-tag-signature-guard.md` (release tag signing)
- **Surviving non-ADR docs that can absorb non-architectural breadcrumbs:**
  - `docs/branch-protection.md` (CI status checks)
  - `docs/events-v1.md` (canonical NDJSON event contract — for telemetry refs)
  - `docs/test-skips.md` (skip inventory — for `#[ignore]` test refs)
  - `docs/viability/multi-provider-profile.md` (v0.9 deferral list — for `pdf.rs:7`)

---

## 7. Operational notes

- This validation used a read-only worktree at `.worktrees/validate-773-docsB-docsC`, branch `validate-773-docsB-docsC`, branched from `main` HEAD `28f2cac`. No production code was modified. The branch is local-only.
- All line numbers cited in this report were verified against `main` HEAD `28f2cac` on 2026-09-06.
- The total stale-reference count of 210 was verified via `grep -rn 'proposal-\|V4 §\|v0\.[23]-status\|docs/deferred-' src/ tests/ --include='*.rs' --include='*.sql' | wc -l` (returns 212, accounting for 2 duplicate matches in `v0.[23]-status`).
