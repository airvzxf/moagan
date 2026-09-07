# Validation report: #769 — `total_sketches` counts `.meta.json` sidecars

**Issue**: [#769 — bug(discover_summary): total_sketches counts .meta.json sidecars, doubling final/summary.json count](https://github.com/airvzxf/moagan/issues/769)
**Pinned SHA**: `02020c111aa707027aec1e2a5e43e12d802cba86` (v0.14.7 HEAD)
**Validated on**: `28f2cac` (current `main` HEAD as of v0.14.8 — line numbers identical to the pinned SHA per `git blame -L 587,596 src/phases/discover_summary.rs`)

---

## 1. Confirmed: the bug is real

The exact code block at `src/phases/discover_summary.rs:587-596` (current `main` HEAD `28f2cac`, identical to the pinned SHA — `discover_summary.rs` was last touched at `830a5b9` and the buggy block was not altered by v0.14.7 or v0.14.8):

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

The predicate checks `extension() == "json"`. The FS layer (`AtomicWriter::write` → `AtomicWriter::meta_path`, `src/atomic/writer.rs:359-363`) appends `.meta.json` to every written artefact:

```rust
pub fn meta_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".meta.json");
    PathBuf::from(s)
}
```

For `sk_aaa.json`, this produces `sk_aaa.json.meta.json`. `Path::extension()` on `sk_aaa.json.meta.json` returns `"json"` (the last extension only), so both files match the predicate and the count is exactly `2 * N`.

The same bug class was already addressed in sibling call sites within the same file:
- `count_facet_lists` (`src/phases/discover_summary.rs:161-176`) — fixed by #569 / PR #570 (`b7129a1`). Predicate has `!name.ends_with(".meta.json")` AND `extension == "json"`.
- `read_category_docs` (`src/phases/discover_summary.rs:127-154`) — has `name.ends_with(".meta.json")` exclusion.
- `read_facet_lists` (`src/phases/discover_summary.rs:283-308`) — has `name.ends_with(".meta.json")` exclusion.

And in adjacent phases:
- `discover_cluster::execute` (`src/phases/discover_cluster.rs:117`) — uses `primary_json_paths`.
- `discover_facet::execute` (`src/phases/discover_facet.rs:131`) — uses `primary_json_paths`.
- `discover_tag::execute` (`src/phases/discover_tag.rs:54`) — uses `primary_json_paths`.

PR #573 (`4da12bc`) introduced `util::primary_json_paths` (`src/phases/util.rs:251-272`) for the trio above, but the commit message also claimed the exclusion was already in `discover_summary`. That claim is true for three call sites (`read_category_docs`, `read_facet_lists`, `read_uncategorized_theses`) and false for two: `read_clusters` and the inline `total_sketches` walk. So the pattern was partially fixed in `discover_summary.rs` but never audited end-to-end. **This is the 3rd time the bug class has surfaced in this file**, after #569 and #573 — the dead-code sweep in #772 should at minimum flag `#[allow(dead_code)] const SKETCHES_DIR / TAGS_DIR` (lines 80-83) as suspicious for the same reason.

---

## 2. Verdict: APPROVE-WITH-MODIFICATIONS

The proposed fix is correct, but a better implementation exists. Pick the issue's **second alternative** (route through `primary_json_paths`):

```rust
let total_sketches = crate::phases::util::primary_json_paths(
    &ctx.run_dir().sketches(),
)?
.len();
```

This is preferred over the issue's primary fix (the inline predicate `!s.ends_with(".meta.json") && s.ends_with(".json")`) for three reasons:

1. **Removes the last inline `extension == "json"` filter from `discover_summary.rs`**, exactly as the issue's secondary block argues. Future directory walks cannot re-introduce the bug.
2. **Matches the rest of the codebase** — `discover_cluster`, `discover_facet`, `discover_tag` all go through `primary_json_paths`. Using it here completes the migration.
3. **One-line change in `discover_summary.rs`** vs. a 5-line closure replacement, which keeps the diff tight and the review shallow.

### Why not `tag_index.tally.len()` (a fourth option not in the issue)

Conceptually tempting — the tally is the canonical count sourced from the tag phase, and reading from it would eliminate the filesystem walk entirely. But it has a regression risk the issue author probably didn't trace:

- `discover_tag` runs in **degraded mode** when every tagging call fails (`src/phases/discover_tag.rs:144-155`). In that branch the tally is hard-coded to `serde_json::Value::Array(Vec::new())` and `kept.is_empty()` triggers the early return. If `discover_summary` then reads `tag_index.tally.len()` as `total_sketches`, the value would be `0` even when sketches exist on disk.
- `discover_summary` is downstream of `discover_tag` only by phase order, not by data dependency. The current FS-walk approach is robust to tag-phase failure (it walks `sketches/` independently), and the fix should preserve that independence.

Stick with the filesystem walk, just with the right predicate.

### Optional improvement: extract a `count_sketch_files` helper

`count_facet_lists` exists as a private helper specifically so it can be unit-tested (see `render_uncategorized_includes_v4_six_ten_sections` fixture at `src/phases/discover_summary.rs:846+` for the test pattern). The fix's test plan in the issue proposes adding a unit test for `total_sketches_excludes_meta_json_sidecars`, but that test cannot reach the inline block in `execute` without standing up the full phase machinery (checkpoint, mock LLM, telemetry).

A small refactor pulls the block into a testable helper alongside `count_facet_lists`:

```rust
/// Count primary sketches under `sketches/`. Excludes the
/// sealed `.meta.json` sidecars that the FS layer writes next
/// to every artefact (mirrors `count_facet_lists`).
fn count_sketch_files(sketches_dir: &Path) -> Result<usize> {
    Ok(crate::phases::util::primary_json_paths(sketches_dir)?.len())
}
```

Then the call site becomes:

```rust
let total_sketches =
    DiscoverSummaryPhase::count_sketch_files(&ctx.run_dir().sketches())?;
```

This adds 4 LOC and the test (`total_sketches_excludes_meta_json_sidecars`) writes 3 `sk_*.json` + 3 `sk_*.meta.json` files, calls the helper directly, and asserts `3`, mirroring the fixture pattern. The helper has the same shape as `count_facet_lists` and `count_contradictions` (lines 161-176 and 182-193), so the file's test surface stays symmetric.

This is a stylistic call, not a correctness requirement. If the cluster wants the smallest possible diff, drop the helper and use `primary_json_paths(...).len()` inline (skip the helper, write the test via `execute` — which is feasible but heavier).

---

## 3. Related bug worth flagging (out of scope for this PR, but in the same file)

While validating, I noticed `read_clusters` (`src/phases/discover_summary.rs:240-263`) has the same bug pattern:

```rust
fn read_clusters(ctx: &RunContext) -> Result<Vec<Cluster>> {
    let clusters_dir = ctx.run_dir().clusters();
    if !clusters_dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&clusters_dir)?
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && p.file_name().and_then(|s| s.to_str()) != Some("index.json")
        })                                          // <-- bug: no `.meta.json` filter
        .collect();
    ...
```

`Cluster` is `#[serde(default)]` (`src/domain/mod.rs:738-739`), so the `.meta.json` sidecar (`ArtifactMeta` payload: `schema_version`, `size_bytes`, `blake3_hex`, `sealed_at_unix`, `crc32c_hex` — none matching Cluster fields) deserialises cleanly into a `Cluster { id: "", label: "", members: vec![], cohesion: 0.0, ... }`. The function then pushes this phantom into the returned `Vec<Cluster>`, and downstream `render_uncategorized` (line 427+) sorts and surfaces it under `## Temas recurrentes`.

This contradicts the issue's "Out of scope" claim that `read_clusters` "was already corrected by #569/#573". It wasn't — PR #573 (commit `4da12bc`) only touched `discover_cluster.rs`, `discover_facet.rs`, `discover_tag.rs`, and `util.rs`. The `read_clusters` call site in `discover_summary.rs` was never audited.

This is a **separate, independent bug** that should be either:
- **(a)** Bundled into the same fix — both are 1-2 line changes in the same file, the test plan already covers fixture setup, and they share the bug class.
- **(b)** Filed as a follow-up to keep #769 strictly minimal.

My recommendation is **(a)**: one PR, one commit, both filters fixed. The issue's claim that `read_clusters` is correct is the kind of drift that #772 (dead-code sweep) and #773 (stale doc sweep) are meant to catch — a sister cluster ticket can pick it up if the issue author disagrees.

---

## 4. Risk analysis

| Risk | Severity | Mitigation |
|---|---|---|
| `total_sketches` halves after the fix | Low (intentional) | Document in the commit body; this is the bug fix, not a regression. Operators reading `final/summary.json` see honest counts. |
| Schema change to `summary.json` | None | Field is `total_sketches: usize`, schema unchanged. |
| Public API change | None | `DiscoverSummaryPhase::execute` signature unchanged. |
| Test coverage gap | Low | New test `total_sketches_excludes_meta_json_sidecars` covers the previously untested FS-walk path. The existing 5 tests in `phases::discover_summary::tests` use hand-built `DiscoverySummary` literals and never asserted on `total_sketches` derived from the filesystem. |
| `degraded` mode (tag phase fails) | None | Fix uses FS walk, independent of tag phase output. Using `tag_index.tally.len()` would have been a regression here — see §2. |
| `read_clusters` regressions | Out of scope but real | See §3. |

The fix is unambiguously safe to land.

---

## 5. Effort estimate

**Single-commit PR with the recommended fix:**

| Component | LOC | Notes |
|---|---|---|
| Fix `total_sketches` block (lines 587-596) | -10 +2 (or -3 +3 with helper) | Inline replacement, no API change. |
| New `count_sketch_files` helper (optional) | +7 | Mirrors `count_facet_lists` shape. |
| New unit test `total_sketches_excludes_meta_json_sidecars` | +35 | Builds 3 real + 3 sidecar files, asserts 3. |
| CHANGELOG entry | +5 | Match style of v0.14.7/v0.14.8 entries. |
| **Total** | **~35-45 LOC** | **1 commit** |

**If `read_clusters` is bundled in (§3):** +6 LOC, same 1 commit.

**Time:** 30-45 minutes (matches `size:XS` label).

---

## 6. Test plan validation

The issue's test plan is sound. Specifically:

1. **New unit test** `total_sketches_excludes_meta_json_sidecars` — feasible. Mirrors `render_uncategorized_includes_v4_six_ten_sections` fixture (lines 846+). Use `std::fs::write` for both `sk_*.json` and `sk_*.meta.json` files. Asserts the helper returns `3`, not `6`.

2. **`cargo test --lib phases::discover_summary`** — 5 existing + 1 new = 6 tests, all green.

3. **`make lint && make build && make test-ci`** — green. The fix is one closure/path replacement; no new clippy lints, no new dependencies.

4. **E2E smoke** `moagan discover --provider mock:mock-model` — verifies `final/summary.json`'s `total_sketches` matches `tag_index.tally.len()` byte-for-byte. This is the existing smoke gate (per AGENTS.md §"Smoke gates"), so it runs unconditionally.

One additional test that would strengthen the fix: an integration test that runs `discover_cluster` and then asserts `read_clusters` returns only the real clusters, not the sidecars (catches the §3 bug for free).

---

## 7. Final verdict

**APPROVE-WITH-MODIFICATIONS.**

1. **Adopt the issue's secondary fix** (route through `primary_json_paths`) over the inline predicate.
2. **Optionally** extract a `count_sketch_files` helper to make the unit test trivial (mirrors `count_facet_lists`).
3. **Strongly recommend bundling the `read_clusters` fix** (same file, same bug class) — either in this PR or as a follow-up to avoid leaving the 4th instance of the pattern unaddressed.
4. **Reject the use of `tag_index.tally.len()`** as the source of truth — it regresses degraded mode.

The fix is correct, low-risk, low-effort, and closes a bug class that keeps resurfacing in this file.
