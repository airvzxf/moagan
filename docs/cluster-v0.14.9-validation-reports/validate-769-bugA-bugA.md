# Validation report — issue #769-bugA

> `bug(discover_summary): total_sketches counts .meta.json sidecars, doubling final/summary.json count`
> EPIC: post-v0.14.8 hygiene + bug cluster (#775) · Sub-issue: #769 (P3, size:XS)
> Pinned SHA: `02020c111aa707027aec1e2a5e43e12d802cba86` (v0.14.7) · Current HEAD on `main`: `28f2cac` (v0.14.8)

## TL;DR

The bug is real and reproducible exactly as described. `src/phases/discover_summary.rs` is **byte-identical** between the pinned SHA (`02020c1`) and the current HEAD (`28f2cac`) — the offending block is at lines 587–596 on both. Both fix sketches in the issue are correct, but I recommend the **second form (`crate::phases::util::primary_json_paths(...).len()`)** because it removes the last inline `extension == "json"` filter from the file and reuses the battle-tested helper that already has two dedicated unit tests. While validating I also found a **related-but-out-of-scope bug** in `read_clusters` (same file, L240) that the issue author incorrectly claims is "already correct" — that should be filed as a separate ticket.

---

## 1. Confirmed — the bug is real

**Location (current HEAD):** `src/phases/discover_summary.rs:587-596`

```rust
let total_sketches = std::fs::read_dir(ctx.run_dir().sketches())?
    .filter_map(|r| r.ok())
    .filter(|e| {
        e.path()
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s == "json")
            .unwrap_or(false)
    })
    .count();
```

`git diff 02020c1..28f2cac -- src/phases/discover_summary.rs | wc -l` → **0**. `git blame -L 587,596 src/phases/discover_summary.rs` on HEAD returns the same `830a5b9` commit and the same eight lines the issue quoted — so the "regenerate the snippet from the pinned SHA" caveat in the issue is unnecessary on `main` as of `28f2cac`.

**Why it doubles.** `AtomicWriter::write` (`src/atomic/writer.rs:120-265`) writes a `<dest>.meta.json` sidecar per write. Its extension is `.json`, so the predicate accepts it indistinguishably. With `N` sketches the walk returns `2 * N`.

**Baseline.** `DiscoverSummaryPhase::read_tag_index` (`src/phases/discover_summary.rs:107-128`) reads `tags/index.json`, which `discover_tag` (`src/phases/discover_tag.rs:165-177`) writes from `all_tags` — built after the `primary_json_paths` filter (`discover_tag.rs:54`). Its `.len()` is `N`, the canonical count.

## 2. Confirmed — the fix sketches work

**Form A (in-place predicate):** correct. `sk_XXX.json` → `ends_with(".json") && !ends_with(".meta.json")` → kept. `sk_XXX.json.meta.json` → rejected. `not-a-json.txt` → rejected. Non-UTF8 filenames → `unwrap_or(false)` → rejected. Returns `N`.

**Form B (route through `primary_json_paths`):** correct. `src/phases/util.rs:251-272` already filters `*.json && !*.meta.json`. Returns `Vec<PathBuf>`, `.len() == N`. `?` propagates IO errors the same way the current `read_dir(...)?` does.

## 3. Better alternative — **recommend Form B**

The issue author lists two forms and prefers Form B. I agree; rationale:

| Criterion | Form A | Form B |
|---|---|---|
| Diff size | 7 lines | 1 line |
| Style match with neighbours | `ends_with(".json")` differs from `count_facet_lists:165-172`'s `extension() == "json"` | Exact match with `discover_cluster:117`, `discover_facet:131`, `discover_tag:54`, `count_facet_lists:165-172`, `read_facet_lists:291-298` |
| Single source of truth | No — copy-pastes predicate | Yes |
| Cost | `.count()` only | `.sort()` + `Vec<PathBuf>` (negligible) |
| Test coverage of predicate | Only via new test | Already covered by `primary_json_paths_excludes_meta_sidecars` (`util.rs:1686`) and `primary_json_paths_only_sidecars_yields_empty` (`util.rs:1725`) |
| Future-proofing | New walks need to know the filter | Bug class eliminated at the source |

**Sketch:**

```rust
let total_sketches =
    crate::phases::util::primary_json_paths(&ctx.run_dir().sketches())?.len();
```

## 4. Concern — `read_clusters` is NOT "already correct"

The issue's "Out of scope" paragraph states `read_clusters` is "already correct". **That is wrong.** `src/phases/discover_summary.rs:240-262`:

```rust
fn read_clusters(ctx: &RunContext) -> Result<Vec<Cluster>> {
    // ...
    .filter(|p| {
        p.extension().and_then(|s| s.to_str()) == Some("json")
            && p.file_name().and_then(|s| s.to_str()) != Some("index.json")
    })                                                 // ← no .meta.json filter
    // ...
}
```

Why still wrong:
1. `discover_cluster` (`src/phases/discover_cluster.rs:215`) writes clusters via `crate::phases::util::write_json` → `AtomicWriter::new().write` (`util.rs:221-227`) → produces `.meta.json` sidecars (`atomic/writer.rs:13`, `meta_path` at L358-363).
2. Sidecar filename `<id>.json.meta.json` — extension is `json` → filter accepts it.
3. `Cluster` is `#[serde(default)]` (`src/domain/mod.rs:740`) → sidecar deserialises silently to `Cluster { id: "", members: vec![], .. }`.
4. `read_clusters` feeds `render_uncategorized` (`discover_summary.rs:392`), i.e. `## Temas recurrentes` in `uncategorized.md`.

**Blame confirms it was never touched by PR #570:**

```
$ git log -L 240,260:src/phases/discover_summary.rs --no-patch --oneline
830a5b9 chore(release): v0.14.0 — version bump (#732)
```

The fix in `b7129a1` (PR #570) only touched `read_category_docs` (L132) and `count_facet_lists` (L162), not `read_clusters`.

**Recommendation:** out of scope for #769, but file as a sibling ticket. Form B for #769 is doubly motivated: it removes the last inline filter, so a follow-up `read_clusters` migration becomes pure mechanical.

## 5. Concern (minor) — `primary_json_paths` doc-comment is misleading

`src/phases/util.rs:248-250` says:

> "Non-`.json` files and the `index.json` summary sidecars are excluded implicitly by the extension filter..."

But the helper's own test (`util.rs:1699-1709`) asserts `index.json` IS in the returned `Vec`. `discover_facet` works around this with an explicit `!= Some("index.json")` filter at `src/phases/discover_facet.rs:133`.

**Impact on #769: zero.** `sketches/` has no `index.json` (verified via `grep`), so Form B behaves identically to Form A on current `sketches/` contents.

**Suggested fix (separate ticket):** rewrite the comment to clarify that `index.json` IS included.

## 6. Risk analysis

| Risk | Severity | Mitigation |
|---|---|---|
| `total_sketches` halves after the fix | Behavioural change — expected | This is the bug fix; no schema change. Document in CHANGELOG. |
| Test flakes on FS race | Low | Existing fixture at `discover_summary.rs:861-908` (`v4_six_ten_sections`) already does this setup pattern — proven. |
| `primary_json_paths` sort overhead | Trivial | Dozens–hundreds of files, sub-ms. |
| `read_clusters` sibling bug | Real, separate | Flagged (§4). Don't bundle. |
| Doc-comment nit | Cosmetic | Flagged (§5). |

No other production path uses `total_sketches`. Verified by `grep -rn 'total_sketches' src/`:
- `domain/mod.rs:869` — field declaration (unchanged)
- `domain/mod.rs:2153,2162` — round-trip test (unchanged)
- `discover_tag.rs:137` — DIFFERENT field in `tags/index.json` (post-filter `paths.len()`, already correct)
- `discover_summary.rs:226` — render format
- `discover_summary.rs:587` — the bug site
- `discover_summary.rs:606` — passed into `DiscoverySummary`
- `discover_summary.rs:760,778` — literal values in render tests

## 7. Effort estimate

**One commit.** Type: `fix(discover_summary): exclude .meta.json from total_sketches count`.

- Production fix: 1 line (Form B).
- New test `total_sketches_excludes_meta_json_sidecars`: ~40 lines, mirror `render_uncategorized_includes_v4_six_ten_sections` (`discover_summary.rs:845-908`).
- CHANGELOG `[Unreleased]`: ~3 lines.

Total ~45 LOC, 1 commit, ~15-30 min local. T1 + T2 green before push. No new deps, no API surface change, no schema change.

## 8. Verdict

**APPROVE-WITH-MODIFICATIONS.** Use **Form B** (`primary_json_paths(...).len()`). Side findings (§4, §5) should be filed as separate tickets in the same cluster (#775) but **not bundled** with #769.

### Suggested commit message

```
fix(discover_summary): exclude .meta.json sidecars from total_sketches count

DiscoverSummaryPhase::execute counted *.json files in `sketches/`
via `extension() == "json"`, which matched the sealed `.meta.json`
sidecars written by AtomicWriter alongside every artefact. Result:
`final/summary.json` reported `total_sketches = 2 * N` for N real
sketches. The same bug class was fixed for sibling sites in #569/PR
#570 and #573, but this block was missed.

Route through `phases::util::primary_json_paths`, the canonical
predicate already used by `discover_cluster`, `discover_facet`,
`discover_tag`, and the same file's `count_facet_lists` /
`read_facet_lists`. Eliminates the last inline `extension == "json"`
filter in `discover_summary.rs`.

Verified against `tag_index.tally.len()` (the post-filter canonical
count): the two now agree.

Refs #769
```

---

## Appendix — citations

| Claim | File | Lines |
|---|---|---|
| Bug block | `src/phases/discover_summary.rs` | 587–596 |
| Pinned SHA == HEAD on this file | `git diff 02020c1..28f2cac -- src/phases/discover_summary.rs` | 0 lines |
| `count_facet_lists` already correct | `src/phases/discover_summary.rs` | 161–176 |
| `read_category_docs` already correct | `src/phases/discover_summary.rs` | 127–153 |
| `read_facet_lists` already correct | `src/phases/discover_summary.rs` | 283–298 |
| **`read_clusters` still has the bug** | `src/phases/discover_summary.rs` | 240–262 |
| `read_uncategorized_theses` correct (uses `strip_suffix(".json")`) | `src/phases/discover_summary.rs` | 359–365 |
| Helper `primary_json_paths` | `src/phases/util.rs` | 251–272 |
| Existing test for `index.json` inclusion | `src/phases/util.rs` | 1686–1717 |
| Misleading doc-comment about `index.json` | `src/phases/util.rs` | 248–250 |
| Sibling call sites of `primary_json_paths` | `src/phases/discover_cluster.rs:117`, `discover_facet.rs:131`, `discover_tag.rs:54` | — |
| Sidecar writer | `src/atomic/writer.rs` | 13–16, 358–363 |
| `Cluster` is `#[serde(default)]` | `src/domain/mod.rs` | 740 |
| `tags/index.json` tally is post-filter | `src/phases/discover_tag.rs` | 54, 171–176 |
| Test fixture pattern to mirror | `src/phases/discover_summary.rs` | 845–908 |
| Existing tests in scope (5) | `src/phases/discover_summary.rs` | 756, 774, 791, 822, 845 |
| Existing tests for `primary_json_paths` | `src/phases/util.rs` | 1686, 1725 |

---

## Summary for parent agent

- **Report path:** (preserved in `docs/cluster-v0.14.9-validation-reports/validate-769-bugA-bugA.md`)
- **TL;DR (5 lines):**
  1. Bug is real and exactly where the issue says: `src/phases/discover_summary.rs:587-596`, file is byte-identical between pinned SHA and HEAD.
  2. Both fix forms work; recommend **Form B** (route through `primary_json_paths(...).len()`) — single source of truth, removes the last inline `extension == "json"` filter in the file.
  3. Side finding: `read_clusters` (same file, L240) **also** lacks the `.meta.json` filter — the issue's "already correct" claim is wrong; file as a sibling ticket, do not bundle.
  4. Minor doc nit: `primary_json_paths`'s doc-comment at `util.rs:248-250` falsely claims `index.json` is excluded — separate ticket.
  5. Verdict: **APPROVE-WITH-MODIFICATIONS**. ~45 LOC, 1 commit, no schema/API/dependency change.
- **Concerns to flag:** the issue's "Out of scope" paragraph contains a factual error about `read_clusters` that the cluster author should be aware of before the merge lands, otherwise the cluster will ship claiming the bug class is fully eradicated when `read_clusters` still has it.
