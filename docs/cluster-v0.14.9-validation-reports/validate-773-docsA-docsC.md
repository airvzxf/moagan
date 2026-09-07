# Issue #773 — Validation Report

**Issue**: `docs: strip stale proposal-{01,02,03,04}-*.md and V4 §... docstring references (~50 sites in src/)`
**Labels**: `docs`, `priority:P3`, `tech-debt`
**Worktree**: `.worktrees/validate-773-docsA-docsC` (branch `validate/issue-773-docsA-docsC`)
**Verdict**: **APPROVE-WITH-MODIFICATIONS** — problem is real, but the proposed fix has four gaps that will trip up the implementer.

---

## 1. Confirmed real

### 1.1 The target docs are gone

`ls docs/` at HEAD `28f2cac` shows only:

```
adr/  branch-protection.md  events-v1.md  test-skips.md  viability/
```

No `proposal-01-concept.md`, no `proposal-02-rust.md`, no `proposal-03-add-ons.md`,
no `proposal-04-cuarta-etapa.md`, no `v0.2-status.md`, no `v0.3-status.md`, and no
`V4-*.md` design doc series. Deletion history is exactly as the issue claims:

- `0e52e7b` (PR #673) — `chore(docs): remove obsolete proposal specs and cli-cheatsheet`
- `27fda5a` (PR #660) — `docs: prune 28 stale docs and fix stale references in 6 docs`

### 1.2 Stale proposal / v0.[23]-status references — actual count

`grep -rE 'proposal-0[1-4]|v0\.[23]-status|docs/proposal-04-cuarta-etapa' src/ tests/`
returns **70 hits across 43 unique files** (issue claims "~50 sites"). Spot-checked
line numbers vs. the issue's findings table:

| Issue citation | Actual location | Status |
|---|---|---|
| `src/checkpoint/human.rs:32` | line 32 | accurate |
| `src/checkpoint/mod.rs:4` | line 4 | accurate |
| `src/cli/mod.rs:786-787` | lines 786–787 | accurate |
| `src/cli/telemetry_cmd.rs:4,1790` | lines 4, 1790 | accurate |
| `src/config/mod.rs:1154,1776` | lines 1154, 1776 | accurate |
| `src/context/mod.rs:14` | line 14 | accurate |
| `src/discovery/contradiction.rs:81` | line 81 | accurate |
| `src/discovery/matrix.rs:3` | line 3 | accurate |
| `src/domain/constraint.rs:1,10,1024` | lines 1, 10, 1024 | accurate |
| `src/domain/mod.rs:707,944,1460,1489` | lines 707, 944, 1460, 1489 | accurate |
| `src/llm/embed/mod.rs:17,86` | lines 17, 86 | accurate |
| `src/llm/prompts.rs:311` | line 311 | accurate |
| `src/llm/retry_budget.rs:9,53` | lines 9, 53 | accurate |
| `src/phases/dag.rs:10-11` | lines 10–11 (file still cites `docs/proposal-03-add-ons.md` §D.2) | accurate (and partially fixed — already adds "ADR 0001 §D-1") |
| `src/phases/discover_cluster.rs:3` | line 3 | accurate |
| `src/phases/gate.rs:96` | line 96 is `// proposal-` (continuation of "proposal-local checks") | **FALSE POSITIVE** — not a stale ref, just hyphenated word break |
| `src/phases/phase.rs:3267-3268` | lines 3267–3268 | accurate |
| `src/phases/propose.rs:17` | line 17 | accurate |
| `src/phases/rank.rs:309` | line 309 | accurate |
| `src/phases/replace.rs:6` | line 6 | accurate |
| `src/phases/synthesize.rs:546` | line 546 | accurate |
| `src/phases/util.rs:1482,2461` | **actually lines 1590, 2626** | **WRONG — off by ~108/165 lines** |
| `src/phases/validate.rs:5,17` | lines 5, 17 | accurate |
| `src/redact/patterns.rs:231,288` | lines 231, 288 | accurate |
| `src/storage/compression.rs:3` | line 3 | accurate |
| `src/storage/migrations/v008_add_ons.sql:4` | line 4 | accurate |
| `tests/integration_pr10_embedder.rs:3` | line 3 | accurate |

Other files the issue cited by glob (not line-number):
`src/research/{allowlist,mod,fetcher,pdf}.rs`, `src/sandbox/{process,mod,allowlist}.rs`,
`src/telemetry/{cross_run_sweep,dashboard,export,verify,retention}.rs`,
`src/validators/{typescript,schema,sql,mod,python,rust}_validator.rs`,
`src/error/mod.rs` — all confirmed to contain stale refs.

### 1.3 Stale V4 § references — actual count

`grep -rE 'V4 §' src/ tests/` returns **143 hits across ~60 unique files**.
Sample locations (representative, not exhaustive):
`src/ranking/stability.rs:3`, `src/ranking/mod.rs:26`, `src/config/mod.rs:911`,
`src/config/mod.rs:916`, `src/config/mod.rs:1329`, `src/config/mod.rs:1334`,
`src/config/mod.rs:1338`, `src/telemetry/dashboard.rs:5`, `src/telemetry/dashboard.rs:27`,
`src/telemetry/dashboard.rs:59`, `src/telemetry/dashboard.rs:122`,
`src/telemetry/export.rs:4`, `src/checkpoint/{mod,human}.rs` (8 hits),
`src/cli/{mod,discover,run,continue_cmd,telemetry_cmd,diff}.rs` (~25 hits),
`src/discovery/{stop_policy,matrix,facet_cache,integrator,contradiction,tagger}.rs`,
`src/phases/{intake,cluster_proposals,synthesize,replace,decompose,validate,discover_summary,discover_cluster,discover_matrix,deliver,clarify,judge,phase}.rs`,
`src/fs_layout.rs` (8 hits for discovery directory conventions), `src/ids.rs:3`,
`src/llm/role.rs:78`, `src/storage/migrations/v014_calls_retry_count.sql:11`,
`src/storage/migrations/v018_saturation_events.sql:8`.

### 1.4 ADR-0003 broken relative-path links — actually worse than the issue claims

The issue says "ADR-0003 still has dangling relative-path links to
`docs/migrations/v0.12-to-v0.13-config.md` (line 11, 17) and
`src/config/dual_mode.rs` (line 11, 197)". Both confirmed, **but the actual line
numbers are off by 4 lines** (and the issue's "line 11" link is the *Supersedes*
line that points at the still-living `src/config/mod.rs` — the *actual* broken
links start at line 15). The full inventory:

| File | Line | Target | Status |
|---|---|---|---|
| ADR-0003 | 15 | `src/config/dual_mode.rs` | deleted (was 866 LOC; deleted in PR #685 / v0.13.1) |
| ADR-0003 | 21 | `docs/migrations/v0.12-to-v0.13-config.md` | deleted (`docs/migrations/` dir never existed) |
| ADR-0003 | 201 | `src/config/dual_mode.rs` | deleted |
| ADR-0003 | 308 | `src/config/dual_mode.rs` | deleted |
| ADR-0003 | 340 | `src/config/dual_mode.rs::tests` | deleted |
| ADR-0003 | 341 | `src/config/dual_mode.rs` | deleted |
| ADR-0003 | 344 | `tests/integration_config_dual_mode.rs` | deleted |
| ADR-0003 | 346 | `docs/migrations/v0.12-to-v0.13-config.md` | deleted |
| ADR-0003 | 349 | `tests/integration_config_dual_mode.rs` | deleted |

So it's **9 broken links in ADR-0003**, not the 4 the issue lists. The issue's
own count is wrong even by its own "line 11, 17, 197" framing.

Note: `ADR-0004-accelerate-legacy-config-removal.md` also cites the same deleted
files (lines 12, 30, 32, 34) — but those citations are *intentional historical*
context ("we deleted `src/config/dual_mode.rs` in this PR"), so they're load-bearing
and should NOT be cleaned up. The same exception does NOT apply to ADR-0003
because the docs in question were removed in a *later* PR (#685), not the ADR's
own scope.

---

## 2. Scope gaps the issue missed

The issue scope is "proposal-{01,02,03,04}-*.md, v0.[23]-status, V4 §…". A
read-only sweep of the deleted-docs census turned up three more classes of stale
references that the cleanup should sweep in the same commit (or at least flag
for the next PR):

### 2.1 Stale refs to other deleted docs (not in scope, but same class)

| Deleted doc | Sites | Notes |
|---|---|---|
| `docs/deferred-v0.9-2026-08-16.md` | `src/research/pdf.rs:7` | "deferred to v0.9" rationale already superseded |
| `docs/discovery-validation-research-2026-08-13.md` | `tests/integration_discover_opencode.rs:3`, `tests/integration_discover_minimax.rs:4` | |
| `docs/pending-items-2026-08-13.md` | `tests/integration_discover_opencode.rs:171`, `tests/integration_discover_minimax.rs:24,174,183`, `tests/integration_discover_deepseek.rs:175` (5 sites) | historical backlog tracker |
| `docs/research-json-structured-output.md` | `src/llm/json_extractor.rs:3` | rationale migrated to doc comments in #724 |

Total: **9 sites across 4 docs**, all deleted by PR #660. Same class of bug;
should be folded in.

### 2.2 `T01-06 §…` references — same class of stale

`grep -rE 'T01-06 §' src/ tests/` returns **100 hits across 51 unique files**.
T01-06 was the original Rust-port task spec, also deleted by PR #660 (along with
the proposal docs). The pattern is identical to `V4 §…` (often co-cited, e.g.
`src/checkpoint/human.rs:1`: "V4 §5.14 + T01-06 §6.5"). The issue does not list
T01-06 — likely an oversight. **Recommend expanding the scope** to also sweep
T01-06 along with V4.

### 2.3 Catalog-decision refs

`grep -rE 'catalog 10-integrada-v0|catalog I\.6|catalog decision 42' src/ tests/`
returns **42 hits**. These cite the deleted `catalog 10-integrada-v0.md`
("catalog 10-integrada-v0 decision 42", "catalog 10-integrada-v0 §D.11.3", etc.)
— same class, but harder to mechanically sweep because the text isn't a doc
filename. Worth flagging as a follow-up; do NOT fold into this PR.

---

## 3. Concerns about the proposed fix

### 3.1 "Replace with surviving ADR" — works for ≤ 5 sites

The issue suggests "replace the deleted-doc citation with the surviving ADR
(`docs/adr/0001..0005`) that owns the design intent, or drop the citation
entirely." Surveying the surviving ADRs:

| ADR | Covers |
|---|---|
| ADR-0001 | no-go list policy (crate admission) |
| ADR-0002 | runtime coverage (`-Cinstrument-coverage`) |
| ADR-0003 | `[[providers.<name>]]` array-of-tables config schema |
| ADR-0004 | v0.13.1 legacy-config removal (acceleration) |
| ADR-0005 | tag signature guard (`verify-tag`) |

Coverage map for the stale references:

- **`src/phases/dag.rs:10-11`**: already partially updated by someone — the
  line reads "ADR 0001 §D-1 (admission policy for `petgraph`); `docs/proposal-03-
  add-ons.md` §D.2". Issue correctly notes only the `proposal-03` half needs
  trimming. One-line edit. **Works.**

- **`src/config/mod.rs:1154,1234,1271,1776`**: cite `proposal-03-add-ons.md §10-integrada-v0`
  and `§D.19.3`. These describe the DeepSeek roster and the `plan_id` snippet.
  ADR-0003 is the closest match (provider config schema). The citation can
  become "ADR-0003" — but the section numbers (10-integrada-v0, D.19.3) have
  no equivalent in the surviving ADR. **Partially works** — drop the section
  number, keep "ADR-0003".

- **All V4 §… references**: V4 was a single design doc series that covered
  everything — pipeline phases (§5.x), discovery (§6.x), dashboard (§8.x),
  telemetry (§9.x), MVP promise (§13.6). The surviving ADRs do **not** cover
  any of those areas. ADR-0001 covers no-go-list; ADR-0003 covers config
  schema; the others cover meta-process. **There is no ADR to redirect V4
  references to for ~140 of the 143 V4 sites.** The implementer will have to
  drop the citation, which is a real loss of context. Recommend at least a
  one-line note in the AGENTS.md or a new `docs/adr/0006-pipeline-architecture.md`
  ADR that *can* serve as the redirect target — otherwise the V4 cleanup is
  net-context-negative.

### 3.2 ADR-0003 "one-line note in **Superseded by** header" fix is inadequate

The issue says: "Add a one-line note in the new `**Superseded by**:` header
(added by PR #770) so the broken links don't surface as render warnings."

The "Superseded by" header was added in commit `f476535` (PR #770). Verified.
But the broken links are at lines 15, 21, 201, 308, 340, 341, 344, 346, 349 —
**not** in the header. `cargo doc` and GitHub's link checker will warn on all
9 broken links regardless of what the header says. The right fix is one of:

1. Convert each broken link to plain text (e.g. drop the `[…](…)` wrapping
   and just write the filename inline).
2. Move the entire "dual-mode deserializer" content (D-3) into a footnote
   inside the "Superseded by" header (since it documents the removed
   subsystem), and delete the body of §D-3.
3. Add a chrome banner at the top: "This ADR is historical — references to
   `src/config/dual_mode.rs` and `docs/migrations/v0.12-to-v0.13-config.md`
   are intentionally pointing at files deleted in v0.13.1 (PR #685,
   ADR-0004)." That converts the warnings into documented-history and
   silences the link checker.

Option 3 is the smallest blast radius and preserves the ADR's audit-trail
value (the dual-mode deserializer is a non-trivial design decision worth
keeping documented). Recommend option 3.

### 3.3 `src/phases/gate.rs:96` is a false positive

The cited line is the wrap-around of "proposal-local checks" — `// proposal-`
(line 95) and `// local checks run.` (line 97). The word "proposal-local" is
not a doc reference. The fix should skip this site.

### 3.4 Line numbers in the issue drift from current HEAD

- `src/phases/util.rs`:1482,2461 → actual 1590, 2626 (file grew by ~108 lines
  since the F2 report's snapshot).
- ADR-0003 lines: claimed 11/17/197, actual 15/21/201.

Likely cause: the F2 report was generated against an older commit. The
implementer must re-grep every cited line before editing.

### 3.5 Catalog-decision refs and `D.x.x` numbering will still feel cryptic

After stripping `proposal-03 §D.13.15` etc., the remaining `D.x.x` references
(e.g. `src/phases/synthesize.rs:546` becomes `K.1 (skip clusters whose
proposals mix hard-incompatible tags)` — the K.1 still has no home). The
"catalog 10-integrada-v0" / D.x.x numbering scheme was *part of* the deleted
docs. Cleanup is fine, but the implementer should be aware that 30+ sites
will lose context, not just doc references.

---

## 4. Risk assessment

| Risk | Severity | Mitigation |
|---|---|---|
| `cargo doc` / `cargo test` red from accidental content deletion | LOW | grep before-and-after; mechanical edits only inside `///` and `//!` doc comments |
| Loss of design intent for V4 §… sites | MEDIUM | Add ADR-0006 (pipeline architecture) as redirect target, OR accept the context loss and document it in the commit message |
| ADR-0003 broken-link annotations accidentally break the ADR's audit-trail value | LOW–MEDIUM | Use option 3 (top-of-ADR banner) rather than stripping links |
| Scope creep into T01-06 + other deleted docs (not in issue body) | LOW | Document the expansion in the commit message; bundle as a single docs commit |
| Pattern drift: implementer uses different replacement strings per file | LOW | Pre-write a style guide in the PR description; example: `Compliance: ADR-0003 (provider roster).` |

The acceptance criterion "`cargo build --all-targets` green; no semantic change"
holds trivially — these are all comment-only edits. The hard part is **getting
the right replacement text** (or accepting the context loss).

---

## 5. Effort estimate

| Bucket | Sites | Style |
|---|---|---|
| Proposal / v0.[23]-status in `src/` | 60 | Per-file one-line edits; commit per module = ~15 commits |
| Proposal / v0.[23]-status in `tests/` | 10 | One commit |
| V4 §… in `src/` | 130 | Larger commits grouped by file (V4 §5.x in `src/phases/` = one commit; V4 §8.x in `src/{config,telemetry,dashboard}/` = one commit) ≈ 6–10 commits |
| V4 §… in `tests/` | 13 | One commit |
| ADR-0003 broken-link annotation | 9 sites in one ADR | One commit |
| Stale `deferred-*` / `pending-items-*` / `discovery-validation-*` / `research-json-structured-output.md` (optional fold-in) | 9 sites | One commit |
| Stale `T01-06 §…` (optional fold-in) | 100 sites | ~10 commits |

**Total minimum scope** (proposal + V4 + ADR-0003): **~225 sites across ~25
commits, ~600–800 LOC touched** (counting both deletions and replacement lines).

**Total with optional fold-in** (adds 110 T01-06 + 9 other): **~340 sites across
~35 commits, ~900–1200 LOC**.

The issue's "One-line commit per file (or batched by module)" guideline implies
~25 commits which matches. The "~50 sites" headline figure understates the
actual scope by 4–7× because the V4 sweep alone is 143 sites. **Recommend
the issue author update the headline number to "~220 sites" before merging
the issue description into a PR description.**

---

## 6. Better alternatives

If we want a single PR that resolves the entire class of stale-doc references:

1. **Mechanical sweep** (what the issue proposes, executed properly):
   - Strip all `proposal-XX` / `v0.[23]-status` / `V4 §…` / `T01-06 §…` /
     `docs/deferred-*` / `docs/pending-items-*` /
     `docs/discovery-validation-*` / `docs/research-json-structured-output.md`
     references from `src/` and `tests/`.
   - For each removed reference, replace with either:
     - the closest surviving ADR (`docs/adr/0001..0005`), OR
     - an inline comment explaining the design intent without a citation, OR
     - nothing (drop the citation entirely).
   - For ADR-0003: add the chrome banner per option 3 in §3.2.
   - Acceptance: `cargo build --all-targets` green; `cargo doc --no-deps`
     produces no broken-link warnings.

2. **ADR-0006 first, then sweep** (preserves more context):
   - Author `docs/adr/0006-pipeline-architecture.md` that consolidates the
     V4 §5 / §6 / §8 / §9 / §13 design into a single normative document.
   - Then sweep `src/`/`tests/` and redirect V4 §… citations to ADR-0006 §…
     instead of dropping them.
   - Cost: 1–2 extra days; benefit: most V4 sites become *more* informative,
     not less.

3. **Single mega-PR with two phases**:
   - Phase 1 (this PR): the strip + ADR-0003 banner.
   - Phase 2 (follow-up, ADR-0006): re-add ADR-backed citations to the most
     context-heavy sites (`src/phases/synthesize.rs`, `src/phases/rank.rs`,
     `src/discovery/*`, `src/telemetry/dashboard.rs`).

**Recommend option 3** — keeps the immediate PR mechanical and reviewable
(no design judgement required), and lets the ADR-0006 follow-up happen on
its own clock without blocking the cluster.

---

## 7. Verdict

**APPROVE-WITH-MODIFICATIONS**.

The bug is real and well-scoped to a single class of cleanup. Four fixes
needed before the issue becomes a clean implementation:

1. **Update headline scope from "~50 sites" to "~220 sites"** (proposal +
   v0.[23]-status + V4 §…, src/ + tests/). V4 alone is 143 sites.
2. **Re-grep every cited line** before editing. At least `util.rs`,
   `gate.rs`, and `ADR-0003` have wrong line numbers vs. HEAD.
3. **Expand the scope to include** `T01-06 §…` (100 sites, same class,
   often co-cited) and the four other deleted-doc refs in §2.1 (9 sites).
   Document the expansion in the commit message.
4. **Replace the ADR-0003 fix sketch** ("one-line note in Superseded by
   header") with a top-of-ADR chrome banner that documents *why* the broken
   links remain (historical audit trail of the v0.13.1 removal). The current
   sketch doesn't address the 9 broken links scattered through the body.

If the maintainer wants to fold T01-06 in, the cluster PR is well-positioned
for it — the validation-tiers #724/#725/#726 work already normalized the
"task-ID-as-citation" convention. If not, scope creep into T01-06 should be
deferred to a separate P3 issue so this PR stays mechanical.

The core claim — that ~50 stale references survive in `src/` doc comments —
is correct in spirit (the actual number is higher because V4 §… wasn't
counted) and the proposed replacement strategy is sound. With the four
modifications above, this becomes a clean ~25-commit mechanical sweep.
