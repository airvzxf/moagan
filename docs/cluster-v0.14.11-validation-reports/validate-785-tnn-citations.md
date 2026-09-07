# Validation report — issue #785 (TNN-NN citations sweep)

> Issue: #785 — `docs: sweep the 33 remaining TNN-NN task-tracker citations in src/ + tests/`
> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Current HEAD on `main`: `ce524e21ee0028da18384092e60925c69e5172c8` (v0.14.10 tag commit)
> Verdict: **APPROVE-AS-IS** — the issue body is accurate down to the
> last citation; the only editorial judgement is per-line prose rewriting.

## TL;DR

The issue is real. `docs/proposal-02-rust.md` was deleted in commit
`0e52e7b` (PR #673, "chore(docs): remove obsolete proposal specs and
cli-cheatsheet"), and 33 surviving `TNN-NN` task-tracker citations in
11 files in `src/` and `tests/` still point at it. The per-file
inventory in the issue body matches the actual code byte-for-byte:
all 33 IDs, all 16 line numbers, all 11 file paths. No TNN-NN appears
in a string literal, identifier, variable binding, or `#[test]` name,
so the sweep cannot change behaviour. The right fix is to **drop the
citations and rewrite each affected line as natural prose** (the same
strategy #779 used for the 109 `T01-06` citations), with one
editorial nuance noted in §3: three lines in `src/llm/embed/` and
`src/llm/circuit_breaker.rs` also co-cite the separate (still-dead)
`catalog 10-integrada-v0` family, and the implementer should pick
whether to drop the catalog prefix too or leave it.

## 1. Confirmed — the count

### 1.1 Total hit count

```
$ rg -o 'T[0-9]{2}-[0-9]{2}' src/ tests/ | wc -l
33
```

Exact match to the issue's headline. The `TNN-NN` regex (`T[0-9]{2}-[0-9]{2}`)
catches every surviving task-tracker citation in `src/` and `tests/`.

### 1.2 Per-file inventory (issue body vs. reality)

| Issue file | Cited IDs | Reality (line : IDs) | Match |
|---|---|---|---|
| `src/domain/constraint.rs` | T02-09, T03-01, T05-10, T08-06, T08-08, T18-04, T19-09 (7) | :11 T02-09; T19-09; T03-01; T18-04 / :12 T05-10; T08-06; T08-08 | ✓ |
| `src/llm/circuit_breaker.rs` | T00-08, T00-09, T03-03, T08-03 (4) | :20 T00-08; T08-03 / :21 T00-09; T03-03 | ✓ |
| `src/llm/embed/mod.rs` | T03-07, T06-04, T09-02 (×2), T09-08, T18-09 (×2) (7) | :17 T09-02; T18-09; T09-08 / :18 T03-07; T06-04 / :72 T09-02; T18-09 | ✓ |
| `src/llm/embed/remote.rs` | T09-02, T18-09 (×2) (3) | :2 T18-09 / :83 T09-02; T18-09 | ✓ |
| `src/ranking/rubric.rs` | T00-03, T05-06, T15-02 (3) | :8 T00-03, T15-02, T05-06 | ✓ |
| `src/redact/patterns.rs` | T13-09, T20-01 (2) | :288 T20-01; T13-09 | ✓ |
| `src/cli/diff.rs` | T01-10, T10-08, T16-01 (3) | :8 T01-10, T16-01, T10-08 | ✓ |
| `src/cli/validate.rs` | T16-01 (1) | :20 T16-01 | ✓ |
| `src/config/mod.rs` | T00-08 (1) | :134 T00-08 | ✓ |
| `src/llm/retry_budget.rs` | T16-06 (1) | :9 T16-06 | ✓ |
| `tests/integration_circuit_breaker.rs` | T00-08 (1) | :2 T00-08 | ✓ |

**Sum: 7+4+7+3+3+2+3+1+1+1+1 = 33 IDs across 16 lines / 11 files.** Every
line number, every ID, every file path the issue lists is accurate.

### 1.3 Distinct ID inventory (23 unique IDs, 33 occurrences)

```
T00-03 (1)   T00-08 (3)   T00-09 (1)   T01-10 (1)   T02-09 (1)
T03-01 (1)   T03-03 (1)   T03-07 (1)   T05-06 (1)   T05-10 (1)
T06-04 (1)   T08-03 (1)   T08-06 (1)   T08-08 (1)   T09-02 (3)
T09-08 (1)   T10-08 (1)   T13-09 (1)   T15-02 (1)   T16-01 (2)
T16-06 (1)   T18-04 (1)   T18-09 (4)   T19-09 (1)   T20-01 (1)
```

23 distinct task IDs, 33 total occurrences. `T18-09` (4 occurrences) and
`T00-08` / `T09-02` (3 each) are the only multi-occurrence IDs; the rest
are single-cite.

### 1.4 Safe-to-edit verification (per issue §"Fix" item 4)

All four checks return **zero hits** (exit code 1, ripgrep convention):

```
$ rg '"[^"]*T[0-9]{2}-[0-9]{2}' src/ tests/        # string literals  → 0
$ rg 'fn .*T[0-9]{2}-[0-9]{2}' src/ tests/         # identifiers      → 0
$ rg 'let .*T[0-9]{2}-[0-9]{2}' src/ tests/        # variable bindings → 0
$ rg '#\[test\].*T[0-9]{2}-[0-9]{2}' src/ tests/   # test names       → 0
```

No `TNN-NN` lives inside a string, identifier, or `#[test]` name. The
sweep is comment-only and cannot change behaviour, matching the
acceptance criterion implicit in `cargo build --all-targets` staying green.

## 2. Confirmed — the cited document is dead

### 2.1 `docs/proposal-02-rust.md` is gone

```
$ ls docs/proposal-02-rust.md 2>&1
ls: cannot access 'docs/proposal-02-rust.md': No such file or directory

$ git log --all -- docs/proposal-02-rust.md | head -5
commit 0e52e7be23b18c7e4f79c5fe03c8afae4d3fc0dd
Author: Israel Roldan <israel.alberto.rv@gmail.com>
Date:   Fri Aug 28 23:07:08 2026 -0600

    chore(docs): remove obsolete proposal specs and cli-cheatsheet (#673)

$ git show --stat 0e52e7b | grep proposal-02
 docs/proposal-02-rust.md                           | 4144 ------------------
```

Confirmed. The doc was 4144 lines and was deleted wholesale by commit
`0e52e7b` (PR #673). The deletion date is **2026-08-28**, ~10 days before
#779 (the `T01-06` sweep in v0.14.10) — meaning #779 left the
sibling-IDs behind on purpose because they were out of scope, exactly
as EPIC #783 documented. #785 is the follow-up the EPIC explicitly
deferred.

### 2.2 Sibling-IDs share the deleted-doc provenance

Every one of the 33 surviving `TNN-NN` IDs was authored under the same
`proposal-02-rust.md` umbrella. The 33 surviving citations are not a
miscellaneous collection — they are literally the remaining residue of
one deletion, in the same way #779's 109 `T01-06` sites were. Same
mechanical fix applies.

### 2.3 Surviving ADRs do **not** cover any cited topic

| ADR | Topic | Covers any TNN-NN citation here? |
|---|---|---|
| ADR-0001 | no-go list / crate admission policy | No |
| ADR-0002 | runtime coverage (`-Cinstrument-coverage`) | No |
| ADR-0003 | `[[providers.<name>]]` array-of-tables config | No (the `T00-08` in `config/mod.rs` is about the circuit breaker, not providers) |
| ADR-0004 | v0.13.1 dual-mode removal | No |
| ADR-0005 | `verify-tag-signature` guard | No |

**There is no surviving ADR to redirect any of the 33 citations to.**
The implementer must drop them or restate the design intent inline
without a citation.

## 3. Confirmed — the fix sketch

The issue proposes three things, all of which are correct:

1. **Drop the citation or retarget at a surviving ADR.** §2.3
   establishes there is no surviving ADR to redirect to, so the only
   viable option is to drop the citation. Retargeting is impossible
   here.
2. **Rewrite the line as natural English prose — do NOT substitute
   whitespace.** Correct. #773's whitespace-substitution strategy is
   exactly what created the 458-line residue tracked by #784; #779
   rewrote lines instead and produced clean output. #785 should mirror
   the #779 strategy, not #773's.
3. **Clean the residue on every line touched.** Automatic. By
   rewriting the line as prose, the residue on the 14 co-located lines
   (per §5.1) is cleaned as a side-effect. No separate pass needed.

The implementer should **not** use `replace_with_spaces` patterns or
regex-based byte-substitution on these lines — that's the
documented-failure strategy that created #784.

## 4. Concerns / corrections to the issue body

The issue body is unusually accurate — no false positives, no missed
sites, no off-by-line drift (compare with #778 in v0.14.10, which
inherited wrong line numbers from a stale report). Only one editorial
nuance to flag:

### 4.1 Co-located `catalog 10-integrada-v0` family — implementation choice, not an error

Three of the 16 affected lines also co-cite the **separate** (also
dead) `catalog 10-integrada-v0` reference family. The issue marks
that family as out of scope (`Out of scope: … The catalog 10-integrada-v0
§… and D.x.y citation families — separate axes, still partially
live.`), so the implementer should leave those prefixes alone on these
three lines:

| File:Line | TNN-NN | Co-citation |
|---|---|---|
| `src/llm/circuit_breaker.rs:20` | T00-08; T08-03 | `catalog 10-integrada-v0` |
| `src/llm/embed/mod.rs:72` | T09-02; T18-09 | `catalog 10-integrada-v0` |
| `src/llm/embed/remote.rs:83` | T09-02; T18-09 | `catalog 10-integrada-v0` |

`src/llm/embed/remote.rs:2` also co-cites the `D.1.3` family
(`path (D.1.3 / T18-09     )`); same — leave the `D.1.3` half alone.
This is consistent with #783's scope decision and the issue's explicit
"out of scope" paragraph; it's a clarification, not a deviation.

### 4.2 `T00-08 §1428-1435` in `tests/integration_circuit_breaker.rs:2`

The test file has one extra wrinkle: the line is
`(catalog D.19.5, T00-08 §1428-1435).` — note the **line-range
suffix** `§1428-1435`. The `TNN-NN` regex still catches it (because the
`T00-08` substring is present), but the suffix `§1428-1435` is the
real load-bearing detail (a line range inside the deleted doc). After
the fix, the line should drop both the ID and the `§1428-1435` range.
Spot-check confirmed: this is the **only** `TNN-NN §…` site among the
33 — the other 32 are bare IDs.

### 4.3 No false positives, no missed citations

The 33 hits are the actual scope. No `TNN-NN` was missed in `tests/`
(no other test file has a match — `rg 'T[0-9]{2}-[0-9]{2}' tests/`
returns only `tests/integration_circuit_breaker.rs:2`); no false
positive in `src/` (no `TNN-NN` substring appears in a real
identifier, log message, error string, or doc-comment that points at
a live source — every match is inside a `//` / `//!` / `///`
doc-comment, and every one of those comments has no surviving citation
target).

## 5. Cross-cutting concerns

### 5.1 Overlap with #784 (whitespace residue) — partial

The #784 ticket tracks ~468 (measured: 458) comment lines in `src/` +
`tests/` that carry 3+ inner spaces from the #773 sweep. The 33 TNN-NN
sites are partly a subset of those residue lines:

| Metric | Count |
|---|---|
| Comment lines with 3+ inner-space residue (`#784` universe, my regex) | 458 |
| …that **also** contain a `TNN-NN` citation | 14 |
| …that **don't** contain a `TNN-NN` citation | 444 |

So **14 of the 16 affected lines** also carry residue from #773. The
other 2 lines (`tests/integration_circuit_breaker.rs:2` and
`src/llm/circuit_breaker.rs:21`) have the `TNN-NN` but no 3+-space
residue. Either way, the implementer will clean the residue on these
lines as a side-effect of rewriting the citation as natural prose (the
#779 precedent), which is exactly the right behaviour.

The remaining 444 #784 residue lines are out of scope for #785 — they
are `V4 §…`, `proposal-NN-*.md`, `catalog 10-integrada-v0 §…`,
`docs/test-skips.md §…` etc., as the #784 ticket itself documents. #785
should **not** attempt to clean those — that's the #784 PR's job.

### 5.2 Interaction with #779 (already-shipped T01-06 sweep) — clean

`T01-06` and the 33 sibling IDs are disjoint sets (verified: zero
overlap between the regex matches and the post-#779 tree). #779 cleaned
the `T01-06` lines and left the sibling-IDs untouched. The sibling-IDs
were explicitly listed as "out of scope" in EPIC #783 ("The ~29
remaining non-T01-06 TNN-NN task-tracker citations in src/+tests/… —
filed as a follow-up"). Note the EPIC body said "~29"; the actual
count is 33 — the EPIC's figure was a pre-#779 estimate that didn't
include every line. **#785 is exactly that follow-up.**

### 5.3 Risk of cascading edits into other families — present but small

The implementer will be tempted to also drop the `catalog 10-integrada-v0`
prefixes on the three co-citing lines in §4.1. Resist the temptation —
out of scope per the issue body, and the three lines read fine with the
catalog prefix dropped. The cluster PR for #785 should touch **only**
the `TNN-NN` substring on each line; everything else stays put.

### 5.4 `cargo doc` link-count baseline (per #784)

Issue #784 established the `cargo doc --no-deps` warning-count baseline
at 80. None of the 16 affected lines contains a markdown link, so the
#785 fix should not change that count. Verify during validation.

## 6. Effort estimate

The issue's "~2 hours" estimate is accurate. Breakdown:

| Bucket | Sites | Per-line effort | Total |
|---|---|---|---|
| `src/cli/{diff,validate}.rs` | 4 IDs / 2 lines | Trivial drop + rewrite | ~10 min |
| `src/config/mod.rs` | 1 ID / 1 line | Drop, fold into existing prose | ~5 min |
| `src/domain/constraint.rs` | 7 IDs / 2 lines | Drop the parenthetical block, fold "10 pairs" into the surrounding prose | ~15 min |
| `src/llm/circuit_breaker.rs` | 4 IDs / 2 lines | Drop "Spec:" prefix block; rewrite the doc-comment to describe the breaker without the catalog ref | ~15 min |
| `src/llm/embed/{mod,remote}.rs` | 10 IDs / 5 lines | Drop "Compliance:" block; keep the `D.1.3` and `catalog 10-integrada-v0` references per §4.1 | ~25 min |
| `src/ranking/rubric.rs` | 3 IDs / 1 line | Drop the "Refs:" trailing list | ~5 min |
| `src/redact/patterns.rs` | 2 IDs / 1 line | Drop the "Compliance:" trailing parenthetical | ~5 min |
| `src/llm/retry_budget.rs` | 1 ID / 1 line | Drop "Compliance:" trailing parenthetical | ~5 min |
| `tests/integration_circuit_breaker.rs` | 1 ID / 1 line | Drop the trailing parenthetical + the `§1428-1435` suffix | ~5 min |
| Validation (fmt-check, guard-deps, lint, build, test-ci, doc-warning-count) | — | — | ~30 min |

**Realistic estimate: ~2 hours**, of which ~30 min is the gauntlet.
The per-line editorial judgement is real but small (each line is one
of five well-known shapes: "Compliance: catalog 10-integrada-v0 (TNN-NN…)",
"Refs: D.x.x, TNN-NN, …", "Inspired by TNN-NN…", "Spec: catalog …", or
"10 pairs from TNN-NN; …").

**Commit strategy**: per-file single-line commits, or one aggregate
`chore(cluster): sweep TNN-NN task-tracker citations (closes #785)` if
the maintainer prefers to mirror #779's single-cluster-commit shape.
EPIC #783 used the single-cluster-commit shape for #779; mirroring
that is consistent.

## 7. Verdict

**APPROVE-AS-IS.**

The issue is real, the count is exact (33/33), the per-file inventory
is byte-for-byte accurate against the current tree, the dead-doc
attribution is correct (`0e52e7b`, PR #673), the safety check
(no TNN-NN in strings/identifiers/test names) is straightforward and
passes, and the proposed fix (drop + rewrite as prose, per the #779
precedent) is the right strategy because no surviving ADR covers any
of the cited topics.

Three things to flag in the implementer's commit message (no
action-required for the issue to be merged):

1. **Editorial scope** — three lines co-cite the `catalog 10-integrada-v0`
   family; leave that family alone. Drop only the `TNN-NN` substring.
2. **`§1428-1435` suffix** — `tests/integration_circuit_breaker.rs:2`
   has `T00-08 §1428-1435`; drop both the ID and the `§…` suffix.
3. **Residue cleanup is automatic** — by rewriting the affected lines
   as natural prose (the #779 strategy, not the #773 byte-substitution
   strategy), the implementer will incidentally clean the residue on
   the 14 co-located lines without having to think about it.

The issue is ready to implement. No corrections to the issue body
required.
