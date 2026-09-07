# Validation report — cluster interactions (v0.14.11)

> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Issues: #785, #786, #787, #788, #789, #790
> Current HEAD on `main`: `ce524e21ee0028da18384092e60925c69e5172c8` (v0.14.10 release commit, PR #793)

## TL;DR

The cluster is **safe to proceed with scope corrections**, but **three of the six issue bodies contain material inaccuracies** that must be caught before implementation begins. The single file-level collision is **`src/cli/diff.rs`** — touched by #785 (header TNN-NN citation sweep at line 8) **and** #789 (consolidation of the `parse_run_id` helper at lines 124-229). The touched lines are disjoint, so no in-place conflict, but the implementer should land #785 first so #789's editor doesn't carry the dead citation into a refactored file. One **doc-link breakage** lurks in #787 option A: `src/phases/phase.rs:1301` (the doc-comment of the surviving `call_uncached_at_temp_for`) references `Self::call_uncached_at_temp` by `[Self::…]` syntax; deleting the shim leaves a broken intra-doc link that `cargo doc` will flag. No code-path interactions; #786, #787, #790 are mechanically independent of each other and of the wider cluster. #788 must run last because the cluster's own CHANGELOG edits land in `[Unreleased]` until the release commit fires the bump — implementing the guard mid-cluster would force a moving-target CHANGELOG, and the guard's first run would need to be calibrated against the cluster's final `[Unreleased]` shape.

**Three material corrections required before implementation:**

1. **#786 — defer to a future cluster.** The `#[allow(dead_code)]` marker was already removed in `fab69aa` (#781 close-out), the "never read" claim is wrong (`set_param_rejections` reads the field via `.lock()`), and the deletion is a SemVer MINOR break (removes a `pub fn` on a `pub` struct re-exported through `src/lib.rs`). The cluster's PATCH framing cannot absorb this.
2. **#789 — scope-limit to `telemetry_cmd.rs` (7 sites) and correct all line ranges.** The "six duplicated parse sites" claim is materially wrong: there are 17+2 actual sites. Routing all of them through one helper requires a context parameter and a different helper signature. Scope-limit for the v0.14.11 cluster; file the rest as a follow-up.
3. **#788 — tighten Assertion 1 (remove the OR clause).** The proposed OR clause would let the pre-fix `CHANGELOG.md` pass the guard, defeating the entire purpose.

**Final cluster composition: #785, #787, #788, #789 (scope-limited), #790 — 5 P3 tech-debt items, ~6-9 hours of focused work. All patch-compatible.**

**Recommended commit order:** #790 → #785 → #787 → #789 → #788 (one commit per sub-issue, single PR following the #783 precedent; squash-merge yields the same single-commit shape on `main`).

---

## 1. File-level overlap matrix

Cells: `-` = no touch, `D` = deletion, `R` = rewrite (prose change), `E` = extract (move), `+` = add, `M` = Makefile edit, `S` = new shell script.

| File | #785 | #787 | #788 | #789 | #790 |
|------|:----:|:----:|:----:|:----:|:----:|
| `src/cli/diff.rs` | **R:8** | – | – | **R:124-125, 219-229** | – |
| `src/cli/telemetry_cmd.rs` | – | – | – | **R:385-387, 413-416, 514-516, 663-668, 997-999, 1675-1680** | – |
| `src/phases/phase.rs` | – | **D:1131-1196, R:1298-1306** | – | – | – |
| `src/phases/discover_contradict.rs` | – | – | – | – | **R:161, 174, D:183-190, +:const PAIR_TOPIC** |
| `src/domain/constraint.rs` | **R:11-12** | – | – | – | – |
| `src/llm/circuit_breaker.rs` | **R:20-21** | – | – | – | – |
| `src/llm/embed/mod.rs` | **R:17-18, 72** | – | – | – | – |
| `src/llm/embed/remote.rs` | **R:2, 83** | – | – | – | – |
| `src/ranking/rubric.rs` | **R:8** | – | – | – | – |
| `src/redact/patterns.rs` | **R:288** | – | – | – | – |
| `src/cli/validate.rs` | **R:20** | – | – | – | – |
| `src/config/mod.rs` | **R:134** | – | – | – | – |
| `src/llm/retry_budget.rs` | **R:9** | – | – | – | – |
| `tests/integration_circuit_breaker.rs` | **R:2** | – | – | – | – |
| `docs/viability/multi-provider-profile.md` | – | **R:61-64** | – | – | – |
| `scripts/check-changelog-release.sh` | – | – | **+:new** | – | – |
| `Makefile` | – | – | **M:guard-deps** | – | – |
| `CHANGELOG.md` | – | – | **R:footnote backfill** | – | – |

(#786 removed from the matrix per the deferral in §0.)

**Single file-level collision:** `src/cli/diff.rs` is touched by both #785 (line 8, the file's module doc-comment) and #789 (lines 124-125 — the `parse_run_id(&run_a)?` calls; lines 219-229 — the helper itself). The lines are **disjoint** (8 vs 124-229), so the edits do not overlap on any byte. **The collision is editor-ordering**: if #789 lands first, the file's diff still carries the dead T01-10/T16-01/T10-08 citation at line 8 when #785 is being implemented; if #785 lands first, the cleaner file is the canvas #789 then edits. Recommendation: **land #785 before #789** (see §5).

**No other file overlaps.** All other files are touched by exactly one issue.

---

## 2. Code-path interaction map

### 2.1 #785 → no interaction

Comment-only change. Verified at the issue-validation report (`validate-785-tnn-citations.md`): `rg '"[^"]*T[0-9]{2}-[0-9]{2}' src/ tests/` → 0 hits; `rg 'fn .*T[0-9]{2}-[0-9]{2}' src/ tests/` → 0 hits; `rg 'let .*T[0-9]{2}-[0-9]{2}' src/ tests/` → 0 hits; `rg '#\[test\].*T[0-9]{2}-[0-9]{2}' src/ tests/` → 0 hits. No string literal, identifier, `let` binding, or `#[test]` name contains a `TNN-NN` substring. Comment-only.

### 2.2 #787 → intra-doc-link breakage (only option A)

If **option A** (delete `call_uncached_at_temp`) is taken, the doc-link at **`src/phases/phase.rs:1301`** becomes broken:

```rust
/// [`Self::call_uncached_at_temp`] but pins the dispatch to
/// the supplied `(section, model_id)` pair. ...
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_uncached_at_temp_for(
```

This is the doc-comment of `call_uncached_at_temp_for` (the surviving function). It references the deleted function by `[Self::…]` rustdoc syntax. `cargo doc --no-deps` will emit a warning and break the cluster's documented invariant of `cargo doc --no-deps warning count does not increase (baseline: 80)`. **Fix is one line** — replace `[Self::call_uncached_at_temp]` with `[Self::call_uncached_at_temp_for]` (cross-reference the surviving sibling) or rephrase the sentence.

If **option C** is taken (keep the shim, add a test), the link stays intact. **Recommendation:** the validation report's option A + the one-line doc-link fix is the lower-risk cluster path because option C requires a new test exercising a shim that has zero existing callers, which is the kind of "tribal" test the cluster pattern explicitly avoids.

### 2.3 #789 → scope inflation, no code-path interaction

`#789`'s only code-path concern is the **scope expansion from 6 sites to 17+2**. The 6 in `telemetry_cmd.rs` are all variants of:

```rust
let x: RunId = raw.parse().map_err(|e| Error::InvalidArgs(format!("invalid run id '{raw}': {e}")))?;
```

— one exception at line 416 uses the different message `"bad run row: {e}"` (a per-row filter inside the `for row in &rows` loop; that message is deliberately different because the value comes from the DB rather than the CLI). The 6 in `cli/mod.rs` use a slightly different format — `format!("{e}")` only, dropping the `invalid run id '<raw>'` wrapper — which is why they were not caught in the original "five inline + one factored" framing. The 2 in `telemetry/dashboard.rs` use `format!("invalid run id '{raw}': {e}")` (HTTP-API variant).

If #789 limits itself to the **7 telemetry_cmd.rs sites** as the validation report recommends, the helper signature stays straightforward and the `parse_run_id` helper at `src/cli/diff.rs:219-229` becomes the obvious home (no module reshuffle). If #789 expands to all 17, the helper needs a context parameter (`Option<&str>` for which-argument-was-bad) or the per-site message variants become separate `parse_run_id_raw` / `parse_run_id_user_input` helpers — both options were raised in the issue's "Check first whether the six sites' error messages are actually identical" paragraph, which is therefore load-bearing.

### 2.4 #790 → no interaction

`pair_topic(_f)` is a private free function in `src/phases/discover_contradict.rs`, called once at line 174, with no symbol exported from the module. The two existing tests `into_contradictions_empty_findings_yields_low_row` and `into_contradictions_maps_findings_to_rows` assert on `severity`, `description`, and `len` only — neither test reads the `topic` field. Either option A (const) or B (zero-arg fn) is behaviour-neutral. The validate-790 report notes an **editorial nuance**: line 161 hardcodes `"consistency".into()` directly in `into_contradictions`, separate from the `pair_topic(f)` call site — option A should harmonise line 161 to also use `PAIR_TOPIC.to_owned()` for consistency, but that's a 1-line extra change.

### 2.5 #788 → no interaction with other issues

The guard script reads `Cargo.toml` (`version` field) and `CHANGELOG.md` (headings + footnote anchors). It does not touch any source code under `src/`, `tests/`, or `docs/viability/`. The only side-effect on the cluster is the footnote backfill in `CHANGELOG.md` (the existing `[0.14.3]`, `[0.14.4]`, `[0.9.1]`, `[0.9.2]` are defined; the missing `[0.14.0]`, `[0.14.1]`, `[0.14.2]`, `[0.14.5]`, `[0.14.6]`, `[0.14.7]`, `[0.14.8]`, `[0.14.9]`, `[0.14.10]` need backfill). The validate-788 report notes **Assertion 1 has a logical flaw** — the proposed OR clause (`only ## [Unreleased] above the newest release heading`) is true in the pre-fix state and would mark the broken `CHANGELOG.md` green. Fix is one line — drop the OR clause or replace with a stricter invariant. **APPROVE-WITH-MODIFICATIONS.**

### 2.6 #786 — DEFERRED OUT

Per the validation report, #786's deletion is a SemVer MINOR break (`set_param_rejections` is `pub fn` on `pub struct BreakeredProvider` reachable via `moagan::llm::provider`). The cluster's PATCH framing cannot absorb it; defer to a future cluster or minor release.

---

## 3. Documentation cross-references

### 3.1 Intra-doc-link audit (rustdoc resolution)

The cluster's `#785` + `#787` collectively modify ~17 lines of doc-comments. `cargo doc --no-deps` walks all `[Type]`, `[`crate::path`]`, and `[Self::method]` references and fails on unresolved links. Verified the following:

| Doc-link | File | Status under cluster |
|-----------|------|----------------------|
| `[`crate::llm::provider::registry_from_config_with_home_and_sink`]` | `src/llm/provider.rs:676, 690` | Unchanged. Safe. |
| `[Self::call_uncached_at_temp]` | `src/phases/phase.rs:1131` (deleted with #787 option A) | Self-contained within the deleted block. Safe. |
| `[Self::call_uncached_at_temp]` | `src/phases/phase.rs:1301` (in `call_uncached_at_temp_for`'s doc-comment) | **BREAKS under #787 option A**. See §2.2. |
| `[`crate::phases::phase::RunContext::call_uncached`]` | `src/llm/wire.rs:55` | References `call_uncached` (NOT `call_uncached_at_temp`). Safe. |
| `[`crate::phases::phase::RunContext::dispatch_to_provider`]` | `src/llm/provider.rs:219, 230` | Unchanged. Safe. |
| Module-level `//!` TNN-NN citations | All 16 lines touched by #785 | All are prose; none use rustdoc link syntax. |

**No other intra-doc-links span files touched by multiple cluster issues.** The `#785` + `#787` doc-touch overlap is empty (different files). The `#785` + `#789` overlap on `src/cli/diff.rs` is editor-ordering only — `cli/diff.rs:8` is `//! Inspired by T01-10     , T16-01      and T10-08:`, plain prose with no rustdoc links.

### 3.2 Doc-file references

`docs/viability/multi-provider-profile.md:61-64` is the only cross-file doc touched by the cluster (by #787). Verified that no other `docs/` file references `call_uncached_at_temp` (the only matches are within the issue body and the validation report under `docs/cluster-v0.14.11-validation-reports/validate-…`, which are audit-trail files the cluster explicitly preserves).

### 3.3 CHANGELOG references

`CHANGELOG.md` is touched only by #788 (footnote backfill + the script's own `[Unreleased]` guard target). No other cluster issue modifies CHANGELOG. The cluster's own release notes land in `[Unreleased]` during implementation, which is what #788's guard assumes.

---

## 4. Test impact map

### 4.1 #785 → no test impact

Comment-only; verified by `validate-785-tnn-citations.md` §1.4.

### 4.2 #787 → no test impact

Direct tests of `call_uncached_at_temp`: **zero**. The function has zero callers anywhere in `src/` or `tests/`. The two closest tests (`phases::phase::tests::call_uncached_*`, the `phase_output_*` family in `discover_matrix`) all exercise `call_uncached_at_temp_for` or `call_uncached`, which are not affected. If option C is taken (keep the shim + add a test), the new test must exercise the shim against a mocked registry — but option C is not recommended.

### 4.3 #789 → assert-only test impact

The cluster's `cmd.dispatch()` tests in `src/cli/telemetry_cmd.rs` assert `matches!(err, Error::InvalidArgs(_))` or `matches!(err, Error::InvalidState(_))` — they do **not** assert on the exact error message text. Verified:

| Test | File | Line | Assertion |
|------|------|------|-----------|
| `list_unknown_run_id_returns_invalid_args` | `src/cli/telemetry_cmd.rs` | 2048 | `assert!(matches!(err, Error::InvalidArgs(_)))` |
| `list_unknown_run_uuid_returns_invalid_state` | `src/cli/telemetry_cmd.rs` | 2061 | `assert!(matches!(err, Error::InvalidState(_)))` |
| `summary_invalid_run_id_returns_invalid_args` | `src/cli/telemetry_cmd.rs` | 2072 | same |
| `summary_unknown_run_returns_invalid_state` | `src/cli/telemetry_cmd.rs` | 2083 | same |
| `compare_invalid_run_id_returns_invalid_args` | `src/cli/telemetry_cmd.rs` | 2095 | same |
| `compare_unknown_run_returns_invalid_state` | `src/cli/telemetry_cmd.rs` | 2107 | same |

If #789's helper preserves the `"invalid run id '{raw}': {e}"` message verbatim, all6 tests pass unchanged. If the helper standardises on a different message (e.g. drops the wrapper), the existing tests still pass — but the docs/tests like `smoke_diff.sh:95` (`"${BIN}" diff not-a-uuid also-not-a-uuid`) and `scripts/smoke_diff.sh`'s grep patterns become silent regressions, since shell-test stability depends on the existing error message text. Recommendation: **preserve the message verbatim** for the 5 sites that share `"invalid run id '{raw}': {e}"`, and let the 1 anomalous site at `telemetry_cmd.rs:416` keep its `"bad run row: {e}"` via a separate helper or a `context` parameter.

`scripts/smoke_diff.sh:95` greps the error output via stderr; the exact text matters here.

### 4.4 #790 → no test impact

The existing tests `into_contradictions_empty_findings_yields_low_row` (`src/phases/discover_contradict.rs:301-312`) and `into_contradictions_maps_findings_to_rows` (`:317-343`) assert on `rows.len()`, `rows[0].severity`, `rows[1].severity`, `rows[0].description`. **No assertion reads the `topic` field.** Either option A (const) or B (zero-arg fn) is behaviour-neutral.

### 4.5 #788 → no test impact on existing tests, adds one new bash-script validation path

The guard script is additive — it runs as part of `make guard-deps` (T0, pre-commit + CI). The validate-788 report specifies three validation gates the new script must satisfy:

1. Fails on pre-fix `CHANGELOG.md` (`git show b549f24:CHANGELOG.md`).
2. Passes on current `main`.
3. Deliberately bumping `Cargo.toml` without touching `CHANGELOG.md` makes the guard fail.

Gate 3 is the load-bearing regression test — it must be exercised manually during PR review and documented in the PR description.

---

## 5. Recommended sequencing

**Order: #790 → #785 → #787 → #789 → #788** (5 commits after deferring #786)

Rationale per step:

1. **#790 (`pair_topic` → const).** Smallest, most isolated, single file. Validated by `validate-790-pair-topic.md` (APPROVE-AS-IS, option A recommended). Zero cross-issue coupling. Land first to warm the cluster branch with a no-risk commit. Also includes the line 161 harmonisation (`"consistency".into()` → `PAIR_TOPIC.to_owned()`) per the validate-790 finding.

2. **#785 (TNN-NN sweep).** Touches 11 files, all comment-only. Must land before #789 because #789 will edit `src/cli/diff.rs` and we want #789's editor to operate on the file with the cleaned-up header. (See §1.)

3. **#787 (delete `call_uncached_at_temp`, option A).** Touches `src/phases/phase.rs` (delete the function + the `// Marker required:` block + update the doc-link at line 1301 to point at the surviving sibling `call_uncached_at_temp_for`) and `docs/viability/multi-provider-profile.md:61-64`. The doc-link fix is the only non-mechanical step. Land before #789 because #789 touches many CLI files and we want the cluster branch's mid-cycle state to be as clean as possible when reviewers see it.

4. **#789 (consolidate RunId CLI parse sites, scope-limited).** Largest refactor — 7 sites in `src/cli/telemetry_cmd.rs` plus the existing helper at `src/cli/diff.rs`. **Must land after #785** so the file the implementer opens (`src/cli/diff.rs`) is already cleaned. The cluster branch's last "real code" commit — any reviewer reading the branch as a sequence will see small dead-code → docs → larger doc-link fix → dedup, in that order, which mirrors the #783 precedent.

5. **#788 (CHANGELOG guard).** Last because the guard's first run on the cluster's branch must see the *final* `[Unreleased]` shape. Land as the final cluster commit so that PR review can verify (a) the script is shellcheck-clean, (b) the three validation gates in the issue pass (after the OR-clause tightening per the validate-788 finding), (c) the footnote backfill is correct, and (d) `make guard-deps` is green. If the cluster branch already ships `[Unreleased]` notes for #785/#787/#789/#790 at the moment #788 lands, the guard's "newest release heading" baseline is the v0.14.10 line — i.e. the script must not flag the cluster's own `[Unreleased]` block as a violation.

**Why not the opposite order?** If #788 lands first, the cluster branch is briefly green even before #785/#787/#789/#790 are committed (the script only fires on Cargo.toml bumps, which the cluster doesn't perform). But the footnote backfill changes CHANGELOG.md, which then becomes a moving target for the other4 commits' PRs. Land last.

---

## 6. Commit granularity

**Follow the #783 precedent: one commit per sub-issue, five commits in a single cluster PR.** The PR will squash-merge into a single commit on `main` (verified by inspecting the v0.14.9 cluster merge: `fab69aa chore(cluster): post-v0.14.9 hygiene cluster (closes #778, #779, #780, #781, #791)` is one commit with 5 issue closures in the body; per-commit history is visible only in the PR's commits tab, not on `main`).

**Per-issue commit subject + body (sketch):**

```
#790 refactor(phases): replace pair_topic with PAIR_TOPIC const
#785 docs: sweep 33 remaining TNN-NN task-tracker citations in src/ + tests/
#787 refactor(phases): delete the uncalled call_uncached_at_temp compat shim
#789 refactor(cli): consolidate RunId CLI parse sites in telemetry_cmd.rs
#788 ci: guard that a release bump renames the CHANGELOG [Unreleased] heading
```

**Should any issues be combined?** No. Each commit has independent tests (or none), independent files (except #785 ↔ #789 on `cli/diff.rs`, which is editor-ordering not commit-merge conflict), and independent revert risk. Combining them would obscure the cluster's "one logical change per commit" principle and make bisect less useful.

**Cluster PR commit message** (template, mirrors `fab69aa`):

```
chore(cluster): post-v0.14.10 hygiene cluster (closes #785, #787, #788, #789, #790)

Cluster validation: 8-reviewer swarm, 3/6 issue bodies materially wrong,
corrections recorded on each issue. #786 deferred to a future cluster
(MINOR API break).

No behaviour change, no public API change, no CLI surface change — a
patch release (v0.14.11) following the established cluster pattern.
```

---

## 7. Risk assessment

### 7.1 Material-error rate

The v0.14.9 cluster PR (`fab69aa`, closes #778-#781, #791) noted: **3 of 4 issue bodies were materially wrong**. Applying the empirical **75% material-error rate** to the v0.14.11 cluster's 6 issues predicted **4–5 issues** need correction. Already-verified discrepancies:

| Issue | Discrepancy | Resolution |
|-------|-------------|------------|
| #786 | Stale evidence (marker already removed), wrong "never read" claim, MINOR API break | **Defer to future cluster** |
| #788 | Assertion 1 has a logical flaw (proposed OR clause would let the pre-fix bug pass) | Tighten per validate-788 finding |
| #789 | "six duplicated RunId parse sites" — actual is **17+2** | Scope-limit to 7 sites in `telemetry_cmd.rs` |
| #785 | **Validated APPROVE-AS-IS** by `validate-785-tnn-citations.md` | None |
| #790 | **Validated APPROVE-AS-IS** (option A) by `validate-790-pair-topic.md`; add line 161 harmonisation | None |
| #787 | "No callers anywhere in src/ or tests/" verified — only references are the doc-link in `:1301` and the `multi-provider-profile.md` doc, both named in the issue's "Fix" item A. The issue missed the `:1134` doc-link inside the deleted block's own doc-comment (harmless — that block goes too). | Low — doc-link fix in #787 commit |

**Conclusion: 3 of 6 issues (#786, #788, #789) need material correction before implementation.** #786 is deferred entirely; #788 and #789 receive one-line fixes (OR clause removal, scope clarification) at implementation time.

### 7.2 Schedule risk

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|-----------|
| #789 scope inflation (if not scope-limited) leads to 1+ day of additional refactor | High | Medium | Scope-limit per validate-789 finding |
| #787 doc-link breakage missed by clippy | Medium | Low (1-line fix) | Run `cargo doc --no-deps` and count warnings; assert baseline 80 |
| #788 Assertion 1 logical flaw not caught | High | High (the guard wouldn't fire) | Already flagged by `validate-788-changelog-guard.md`; implement the validated modification |
| #785 leaves #784's whitespace residue on lines NOT in the 11 TNN-NN files | Certain | None for this cluster | Document in #785's PR description; #784 remains the responsible ticket |
| Mid-implementation blocker | Low | Low | Each commit is independent; revert safety per §7.3 |

### 7.3 Rollback safety

Each issue is independently revertable:

- #790: revert restores the `pair_topic` function and the call-site change. Two-line diff, zero risk.
- #785: revert restores the 33 TNN-NN strings. Cosmetic only.
- #787: revert restores the shim.1-line (re-add the fn) + doc-link fix. Marker must also be re-added (currently absent on `main` per the v0.14.10 cluster's removal).
- #789: revert restores the7 inline parse blocks. Helper at `cli/diff.rs` stays; tests still pass.
- #788: revert removes the guard script and unwires the Makefile. CHANGELOG footnote backfill can stay (it's a pure documentation improvement) or be reverted (1-line per footnote).

---

## 8. Cross-cluster notes

### 8.1 #784 (deferred, size L — whitespace residue)

#784 is the **~468 comment lines** with `TNN-NN` whitespace residue left by the #773 sweep, plus **1 broken rustdoc link** at `error/mod.rs:290`. The cluster pattern (delivered in `fab69aa`) explicitly deferred #784 to a future cluster.

**Cross-cluster interaction with #785:** Yes, **meaningful and useful**. The 16 lines #785 rewrites in `src/domain/constraint.rs`, `src/llm/circuit_breaker.rs`, `src/llm/embed/{mod,remote}.rs`, `src/ranking/rubric.rs`, `src/redact/patterns.rs`, `src/cli/{diff,validate}.rs`, `src/config/mod.rs`, `src/llm/retry_budget.rs`, and `tests/integration_circuit_breaker.rs` are precisely the lines that carry the `#773` whitespace residue. Example (current state):

```rust
src/ranking/rubric.rs:8
//! Refs: D.7.4, T00-03      , T15-02     , T05-06.
```

The pattern `T00-03      , T15-02     , T05-06.` has multi-space padding around the comma — this is **exactly the #784 residue**. When #785 rewrites these lines as natural prose (per the issue's "drop or retarget" instruction), the residue on those 16 lines is cleaned **as a side effect** of the TNN-NN sweep. #784's remaining scope shrinks by ~16 lines (from ~468 to ~452).

**Recommended PR-description note for #785:** "This sweep incidentally cleans the `#773` whitespace residue on the 16 lines it touches. The remaining ~452 #784 residue lines (the body of the codebase not affected by TNN-NN citations) are out of scope; #784 remains the responsible ticket."

### 8.2 #784's broken rustdoc link (`error/mod.rs:290`)

The `error/mod.rs:290` link is unrelated to the cluster's files (it's an intra-`error` reference, not anything the cluster touches). No interaction.

### 8.3 EPIC for the cluster

There is **no EPIC issue** for the v0.14.11 cluster (verified by `gh issue list --search "v0.14.11 in:title"` → 0 hits). The v0.14.9 cluster had #783 as its EPIC with the "one commit per sub-issue" guidance documented. **Recommendation:** the cluster PR description should mirror that guidance and explicitly cite the #783 EPIC's "Strategy" section as the precedent (one commit per sub-issue + single squash-merged cluster PR).

### 8.4 Cargo features / cfg interaction

Verified: the cluster touches no `#[cfg(feature = "...")]` gates. `cfg(feature = "coverage")` only gates `src/main.rs:162` and `src/coverage/mod.rs:114,132`. None of these are cluster files. **No hidden feature-flag dependencies.**

The `dag` feature flag (which gates `petgraph`) likewise is untouched. The cluster works identically under `cargo build`, `cargo build --all-targets`, `cargo build --features dag`, `cargo build --features coverage`. The CI matrix (`.github/workflows/ci.yml`) is unaffected.

### 8.5 v0.14.10 cluster fallout

The v0.14.10 cluster (commit `fab69aa`) closed #778-#781, #791, and explicitly deferred #784-#790 with their ticket numbers in the commit body. **All7 deferred tickets are now the v0.14.11 cluster**, and the cluster author has read the v0.14.10 commit message (the deferral context is in `CHANGELOG.md` lines 90-93 plus the deferred-tickets list at the end of `fab69aa`'s body). No work was lost in the deferral.

---

## 9. Recommendation

**APPROVE-WITH-MODIFICATIONS.** Proceed with the v0.14.11 cluster, but **apply three issue-body corrections before implementation begins**:

### 9.1 Required corrections

1. **#786 — defer entirely.** The MINOR API break conflicts with the cluster's PATCH framing. Add a note to the issue body explaining the deferral and link to the validate-786 report.
2. **#788 — tighten Assertion 1.** Drop the OR clause. The guard must require that every released version in `Cargo.toml` has a matching `## [X.Y.Z]` heading in `CHANGELOG.md`. The pre-fix state has `Cargo.toml: version = "0.14.9"` but no `## [0.14.9]` heading → guard fires correctly. The validate-788 report already specifies the fix.
3. **#789 — clarify scope.** The issue body says "six duplicated RunId parse sites"; the real count is **17 in `src/cli/` + 2 in `src/telemetry/dashboard.rs`**. Adopt framing **(a)** for the v0.14.11 cluster: scope-limit to the **7 sites in `src/cli/telemetry_cmd.rs`** behind the existing `parse_run_id` helper at `src/cli/diff.rs:219`. File the 9 remaining sites as a follow-up.

### 9.2 Cluster branch workflow

1. Branch from `main` at `ce524e2` (current HEAD).
2. Land5 commits in order: #790 → #785 → #787 → #789 → #788. Each commit must pass `make fmt-check guard-deps lint build test-ci`.
3. CHANGELOG.md `[Unreleased]` block accumulates notes across #785/#787/#789 (the cluster's behavioural-neutral changes); #788 does NOT add a CHANGELOG entry because it's a CI-script addition, but it does backfill the missing compare-link footnotes.
4. Open PR with body that mirrors the `fab69aa` template: per-issue summary, material-error table (empty since the corrections are made pre-implementation), validation tier summary (`make test-ci` green, `cargo doc --no-deps` at 80-warning baseline, smoke gates pass).
5. Squash-merge with conventional subject `chore(release): v0.14.11 — Cargo.toml version bump (#<PR>)`.

### 9.3 Final actionable items (in order)

| # | Action | Owner | Blocking? |
|---|--------|-------|-----------|
| 1 | Add deferral note to #786 body | Orchestrator | No (defer happens here, not in cluster) |
| 2 | Update #788 body to drop the OR clause in Assertion 1 | Orchestrator | **Yes** — guard must not pass the pre-fix state |
| 3 | Update #789 body to clarify scope (option (a) recommended) | Orchestrator | **Yes** — affects implementation |
| 4 | Land #790 commit (with line 161 harmonisation) | Cluster branch | No |
| 5 | Land #785 commit + note in PR description about #784 residue cleanup | Cluster branch | No |
| 6 | Land #787 commit (option A + doc-link fix at `phase.rs:1301`) | Cluster branch | No |
| 7 | Land #789 commit (scope-limited to telemetry_cmd.rs) | Cluster branch | No |
| 8 | Land #788 commit (modified script per #2 + footnote backfill) | Cluster branch | No |
| 9 | PR review verifies `make test-ci` green, smoke gates pass, `cargo doc --no-deps` baseline 80 | Reviewer | Yes — pre-merge |
| 10 | Squash-merge, tag `v0.14.11` (follow the AGENTS.md tag-reachability invariant) | Release | Yes — post-merge |
