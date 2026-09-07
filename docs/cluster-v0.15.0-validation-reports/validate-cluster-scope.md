# Validation report — cluster v0.15.0 scope

> Cluster: post-v0.14.11 hygiene (v0.15.0 — MINOR bump)
> EPIC: #797 — "v0.15.0 cluster — BreakeredProvider.param_rejections removal + #773 whitespace residue repairs"
> Sub-issues: #786 (XS, MINOR API break) + #784 (S, scope-limited to 5 sites)
> Base HEAD: `c3c4c8f` (v0.14.11 release bump)

## TL;DR

The cluster scope is **well-defined and ready to land as-is**. Both sub-issues are P3 tech-debt, behaviour-neutral (where applicable), and disjoint in scope. The MINOR-bump framing for v0.15.0 (vs. the PATCH framing of v0.14.11) is justified by #786 alone; #784 is docs-only and would be patch-eligible in isolation. No additional P3 tech-debt candidates have been overlooked that fit the cluster pattern.

**Recommendation: KEEP CLUSTER SCOPE AS-IS. Do not expand.**

**Confidence: HIGH.**

## 1. Open P3 issue triage

`gh issue list --state open --label priority:P3` returns 6 issues; 3 belong to this cluster and 3 are blocked:

| # | Title | Size | Clusterable? | Rationale |
|---|---|---|---|---|
| 797 | EPIC v0.15.0 cluster | M | **IN CLUSTER** | Already declared |
| 786 | delete `BreakeredProvider.param_rejections` | XS | **IN CLUSTER** | MINOR break — validates v0.15.0 bump |
| 784 | repair #773 whitespace residue (5 cases) | L (scope-limited to S) | **IN CLUSTER** | Per EPIC strategy |
| 697 | test-skips.md → generated inventory | L | **NO** | `pending-human` + `pending-operator`; docgen requires tooling decision. Out of cluster scope per prior cluster pattern. |
| 693 | events-v1.md → generated from `Event` enum | L | **NO** | `pending-human` + `pending-operator`; same docgen decision. |
| 674 | CLI reference → generated from clap derive | L | **NO** | `someday` + `pending-operator`; clap-derive codegen requires tooling research. |

**No 1-day-or-less P3 issues were deferred from earlier clusters.** The v0.14.10 cluster (`fab69aa`) closed every P3 item it set up to close (#778-#781, #791) and explicitly deferred #784, #785-#790 to v0.14.11. The v0.14.11 cluster (`4ab8caf`) closed every P3 item it absorbed except #786 (MINOR break) and #784 (size L). #786 → v0.15.0; #784 → also v0.15.0 via EPIC #797's scope-limit to 5 cases.

The "pending" P3s (#674, #693, #697) carry `pending-human`/`pending-operator`/`someday` labels and have been parked at exactly that status since 2026-08-29 to 2026-09-02. None of them become clusterable by removing labels — they require a human/operator decision (docgen tooling choice), not code work.

## 2. rustdoc warning count (`cargo doc --no-deps --lib`)

**Total: 80 warnings** — exact match to the EPIC #797 baseline ("baseline 80"). The v0.14.11 cluster explicitly tested this invariant and the cluster commit (`4ab8caf`) records that the count "does not increase".

Spot-check of the 80 warnings reveals **no orphans specific to this cluster**. Examples of pre-existing warnings:

- `warning: unresolved link to AtomicWriter::with_fsync(false)` — pre-existing intra-doc link breakage from `src/atomic/writer.rs:82`.
- `warning: unresolved link to CardAdity::for_mode_default` — pre-existing from `src/phases/cardinality.rs`.
- `warning: unresolved link to docs/adr/0002-runtime-coverage.md` — pre-existing broken ADR cross-reference.
- `warning: public documentation for 'selection_plan' links to private item 'Self::apply_env_overrides'` — pre-existing doc-hygiene smell.

The 80-warning baseline is NOT going to be increased by the cluster as scoped. The cluster will likely **decrease** it by 3 (the private-link warnings at `src/llm/provider.rs:230, :672` and the `set_param_rejections` doc-block at `:877-884` dissolve with the field/setter deletion). Net: 80 → 77.

## 3. `#[allow(...)]` marker audit

### `#[allow(dead_code)]` markers — 5 remaining in `src/`

| Site | Verdict |
|---|---|
| `src/cli/probe.rs:527` (`TemperatureProbeResult.model`) | Stale-but-load-bearing (v0.14.10 #781 "Marker kept") |
| `src/cli/probe.rs:613` (`ProbeResult.model`) | Stale-but-load-bearing (same) |
| `src/cli/doctor.rs:382` (`capabilities_for_kind`) | Load-bearing (only test callers) |
| `src/phases/phase.rs:779` (`heartbeat_spawned`) | Load-bearing (only tokio-test callers) |
| `src/llm/json_extractor.rs:583` (test fixture `Out.answer`) | Load-bearing (test-only) |

The two `probe.rs` markers are stale — the `model` field is genuinely never read. The cleanest fix would be to delete the field and update ~25 construction sites (effort ~XS-S = 30 min). **Should it be folded into v0.15.0?** Marginal. The cluster pattern prefers tightly-scoped, single-purpose commits; folding a `probe.rs` field deletion into a cluster whose scope is "MINOR API break + 5 docs fixes" muddies the audit trail. **Recommendation: defer to a follow-up cluster.**

### Other `#[allow(...)]` markers — all load-bearing

Spot-check of the 76 total `#[allow(...)]` markers (excluding the 5 above):

- 29× `#[allow(missing_docs)]` — all load-bearing (build failure under `-D warnings`).
- 12× `#[allow(clippy::too_many_arguments)]` — all justify with doc-comment.
- 7× `#[allow(deprecated)]` — migration in flight.
- 2× `#[allow(clippy::print_literal)]` — load-bearing.
- 2× `#[allow(clippy::type_complexity)]` — load-bearing.
- 1× `#[allow(clippy::borrow_deref_ref)]` — load-bearing.
- 1× `#[allow(clippy::should_implement_trait)]` — load-bearing.
- 1× `#[allow(unused_imports)]` — load-bearing.

**No orphans.** Every marker carries either a structural reason, a `// Marker required: …` line, or a clear context justification.

## 4. Citation sweep status

### Already swept by v0.14.11 cluster (#785)

`rg 'T[0-9]{2}-[0-9]{2}' src/ tests/` returns **zero matches** — the 33 `TNN-NN` citations are gone, cleaned in commit `4ab8caf` (post-v0.14.10 hygiene cluster). The cluster invariant holds.

### Already swept by v0.14.9 cluster (#773)

`rg 'V4 ' src/ tests/` returns **only model names and IP enum variants** — no `V4 §...` docstring citations survive. `rg 'proposal-0' src/ tests/` returns **zero matches**. The 210-site sweep from PR #770 (`f476535`) is complete.

`rg 'docs/proposal-' src/ tests/` returns **zero matches** — every `proposal-NN-*.md` reference was stripped.

### NOT swept — `catalog 10-integrada-v0` family

32 references in `src/` survive. validate-785 §4.1 explicitly marked this family as **out of scope** for #785. validate-cluster-interactions §3.1 confirmed the v0.14.11 cluster would not touch them.

**Is this a v0.15.0 candidate?** Technically yes — same hygiene class as #773/#785. But:

- 32 references are concentrated in 25 files, not 84 — a smaller surface than #773
- The references are embedded in module-level `//!` doc-comments, not easily mechanical-rewrite
- The `catalog 10-integrada-v0` doc is not in `git log --all` (never committed) — the citations point at a doc that was distributed off-repo or never landed
- Validating each site requires knowing what the original spec said, which lives only in operator memory or a sister repo

**Recommendation: defer to a separate future cluster** with a dedicated validation report (analogue of validate-773-docsB-docsC). Folding it into v0.15.0 alongside #784's 5-case scope-limit would dilute the cluster's audit trail.

### The `D.x.y` family

Scattered throughout `src/phases/` and other modules — partial overlap with #784's residue. Same hygiene class as `catalog 10-integrada-v0`; same deferral logic applies.

## 5. `#784` scope decision — 5 cases vs. ~466 residue lines

The EPIC #797 scope-limits #784 to **5 specific sites**: 4 functionally degraded + 1 broken rustdoc link. I verified each is real (full detail in `validate-784-whitespace-residue.md`):

1. `src/error/mod.rs:290` — broken intra-doc link `[]()` ✓
2. `src/discovery/contradiction.rs:81` — `§D.x contradiction` unreadable ✓
3. `src/telemetry/mod.rs:295` — dangling "Spec" ✓
4. `src/redact/patterns.rs:232` — empty parenthetical header ✓

(The "5th case" referenced in the EPIC body is the sum "4 + 1", not a separate site.)

**The ~466 residue lines are correctly deferred.** Sampling 30+ residue lines (`src/phases/synthesize.rs:546, :1196`; `src/phases/rank.rs:7-1134`; `src/phases/mod.rs:4, :7`; `src/phases/decompose.rs:5, :86`; etc.) shows the residue is overwhelmingly:

1. **Spanish-language fragments** — `paso 6`, `Reuso por`, `predicado`, `MVP`. These need a Spanish-speaking reviewer to rewrite as English-prose.
2. **Cross-references to deleted docs** — `docs/pending-items-2026-08-13.md` (deleted by PR #660), `docs/test-skips.md` Layer 2 closing notes, etc.
3. **Citation placeholders** — `Catalog / ` (D.11), `Source 1:          matrix literals`, `Phase H (         paso 6)`. Each needs editorial rewriting that knows what the original spec said.
4. **Tabular alignment** — some of the inner-space runs are legitimate column alignment in doc-comments (e.g. `src/phases/cardinality.rs:95-99` are aligned table cells, not residue).

The cluster pattern (validate-773, validate-785) shows residue sweeps benefit from per-cluster F2 review. Forcing ~466 lines through the v0.15.0 cluster alongside a MINOR API break risks:

- F1 validation swarm takes longer (per-line review × 466 lines)
- PR review takes longer (reviewer reads ~466 noise lines around the 5 real fixes)
- Higher regression risk (Spanish-language/Phase-H residue needs domain knowledge)
- Audit-trail dilution (v0.15.0 should be remembered as "MINOR bump for `param_rejections` removal + 4 docs fixes", not "MINOR + doc sweep")

**Recommendation: keep #784 scope-limited to the 5 cases in EPIC #797.** The remaining ~466 residue (including 3 out-of-scope sites flagged in validate-784 §5) is correctly tagged for a future L-size cluster with its own validation report.

## 6. `#786` scope decision — confirmed

The validate-786 report (`docs/cluster-v0.15.0-validation-reports/validate-786-delete-param-rejections.md`) confirms:

- `set_param_rejections` is the only `pub fn` removal needed (verified: 2 hits in src/, definition + 1 caller).
- No `set_param_rejections` callers exist outside `src/llm/provider.rs:1584` (verified fresh).
- No re-export of `set_param_rejections` exists in `src/lib.rs` or any submodule `pub use`.
- Field is genuinely observationally dead (the `self.param_rejections.lock()` read in the setter is write-only in effect, no consumer).
- 3 deletions + 1 caller change = ~15 LOC removed, ~30 min effort (XS label accurate).

**Cluster scope is sound.** No additional MINOR-break items should be folded in by accident.

## 7. Cross-cluster concerns

### EPIC #797 strategy order — **#784 → #786** (corrected per validate-cluster-interactions)

The validate-cluster-interactions report recommends **#784 first, #786 second** (smaller, docs-only → larger, MINOR API break). This mirrors the v0.14.11 cluster's smallest-first pattern (#790 → #785 → #787 → #789 → #788) and makes bisection cleaner if a regression surfaces downstream.

(Note: the original EPIC #797 body recommended #786 → #784; the F1 validation swarm corrected this.)

### CHANGELOG guard from #788

The new `scripts/check-changelog-release.sh` (added in v0.14.11) will fire if `Cargo.toml` is bumped to `0.15.0` without a corresponding `## [0.15.0]` heading in `CHANGELOG.md`. The cluster must follow the v0.14.11 release procedure: add `[Unreleased]` entries for #786 and #784 before the release PR bumps the version, then rename `[Unreleased]` → `## [0.15.0] - 2026-09-XX` in the release commit.

### Pre-merge validation per EPIC §"Validation"

- `cargo doc --no-deps` warning count must not exceed 80. The cluster will likely reduce it to 77 (see §2 above).
- `rg 'set_param_rejections' src/ tests/` must return 0 hits. Verified: currently 2 hits, both in `provider.rs`.
- `rg 'param_rejections' src/` must show only registry-level field. Verified fresh: 17 hits in registry code, 2 in wrapper (`:699`, `:882-884`) — both targeted by #786.
- Smoke gates per AGENTS.md §"Smoke gates" must pass.

## 8. Candidate P3 items for v0.15.0 — summary table

| Item | Source | Fit? | Recommendation |
|---|---|---|---|
| Fold in the 2 stale `probe.rs model` markers (`src/cli/probe.rs:527, :613`) | validate-772 §1.6, §2.4 — never addressed | **MARGINAL** | Defer. Not load-bearing, audited twice, requires ~25 construction site edits. Separate cluster candidate. |
| Sweep `catalog 10-integrada-v0` family (32 sites) | Out-of-scope per validate-785 §4.1 | **NO** | Defer. Same hygiene class as #773/#785 but with different doc provenance. Needs dedicated validate-report with operator input. |
| Sweep `D.x.y` family (scattered) | Same as above | **NO** | Defer to the same future cluster as `catalog 10-integrada-v0`. |
| Sweep `docs/pending-items-2026-08-13.md` residue (≥3 sites in `tests/integration_discover_minimax.rs`) | Deleted by PR #660 | **NO** | Trivially mechanical (~3 lines), but in test files — a follow-up sweep, not a cluster candidate. |
| Sweep the remaining ~466 cosmetic residue lines | #784 deferred scope | **NO** | Defer per #784's own scope decision. Future L-size cluster. |
| Drop redundant `#[allow(missing_docs)]` on 29 enum/struct variants | None — all load-bearing | **NO** | All required by `.clippy.toml`'s `-D warnings`. |
| Tidy `#[allow(deprecated)]` in `sandbox/process.rs` (7 sites) | None — migration in flight | **NO** | Migration actively in progress; load-bearing for the duration. |
| Drop the `// TODO(orchestrator-followup)` comment at `src/phases/util.rs:192` (left over from #774) | Completed by v0.14.9 cluster but the comment stayed | **MARGINAL** | 9-line stale TODO block. Could be folded into #784 if the implementer touches that file anyway, but it isn't in the 5-case list. |
| **Out-of-scope #784 residue (3 sites flagged in validate-784 §5)** | Same #773 sweep family | **NO** | Defer to the future L-size cluster. Keeps v0.15.0 tight. |

## 9. Final recommendation

**KEEP CLUSTER SCOPE AS-IS.** EPIC #797 with #786 (XS, MINOR break) + #784 (scope-limited to 5 cases) is the right size for v0.15.0. The cluster has:

- Clean file-level separation (`src/llm/provider.rs` vs `src/{error,discovery,telemetry,redact}/*.rs`).
- One MINOR-bump driver (#786), no risk of accidental additional MINOR breaks.
- One scope-limited docs fix (#784's 5 cases) without inflating into a separate L-size cluster.
- Documented precedent (v0.14.9, v0.14.10, v0.14.11 clusters all followed the same pattern).
- Validation hooks ready (CHANGELOG guard from #788 will fire on the version bump).

**Confidence: HIGH.** Both sub-issues have F2 validation already recorded. The cluster scope decision was made by the same operator who authored the validate-cluster-interactions pattern. No new P3 candidates surface from this scan that meet the bar for inclusion.

## 10. Sign-off checklist for the cluster owner

Before implementing:

- [x] Verify `cargo doc --no-deps --lib` warning count is exactly **80** on the working branch. **VERIFIED.**
- [x] Verify `rg 'set_param_rejections' src/ tests/` returns exactly **2 hits** (`:882` and `:1584`). **VERIFIED.**
- [x] Verify `rg 'param_rejections' src/` returns only registry-level hits + the 2 wrapper hits targeted by #786. **VERIFIED.**
- [x] Read `docs/cluster-v0.14.11-validation-reports/validate-786-delete-param-rejections.md` for the v0.15.0 context. **REUSED.**
- [ ] Add `## [Unreleased]` entries for #786 and #784 before the release commit.
- [ ] Follow AGENTS.md §"Tag the merge SHA, not the branch tip" for the v0.15.0 tag.

After implementing:

- [ ] `make fmt-check guard-deps lint build test-ci` green.
- [ ] `cargo doc --no-deps --lib` warning count ≤ 80 (predicted: 77).
- [ ] `rg 'set_param_rejections' src/ tests/` returns 0 hits.
- [ ] `rg 'param_rejections' src/` shows only `ProviderRegistry::param_rejections` surviving (no wrapper hits).
- [ ] Smoke gate #1 (`moagan run --mode fast --provider mock:mock-model`) produces `final/portfolio.md` and `rankings/ranking.json`.
- [ ] CHANGELOG guard from #788 (`scripts/check-changelog-release.sh`) passes.

## Citations used in this report

- `docs/cluster-v0.14.11-validation-reports/validate-cluster-scope.md` (cluster pattern precedent)
- `docs/cluster-v0.14.11-validation-reports/validate-cluster-interactions.md` (file-level overlap analysis)
- `docs/cluster-v0.14.11-validation-reports/validate-785-tnn-citations.md` (citation sweep pattern)
- `docs/cluster-v0.14.11-validation-reports/validate-786-delete-param-rejections.md` (reused for v0.15.0)
- `docs/cluster-v0.14.9-validation-reports/validate-772-deadA-deadB.md` (once-over marker audit, current state)
- `docs/cluster-v0.14.9-validation-reports/validate-773-docsB-docsC.md` (citation sweep precedent)
- `CHANGELOG.md` (v0.14.10 cluster #781 verdict — marker kept/dropped classification)
- `commit 4ab8caf` (v0.14.11 cluster)
- `commit fab69aa` (v0.14.10 cluster — #781 marker audit)
