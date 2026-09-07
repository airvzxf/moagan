# Validation report — cluster scope and priority (v0.14.11)

> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Issues: #785, #786, #787, #788, #789, #790
> Current HEAD on `main`: `ce524e2` (`chore(release): v0.14.10 — Cargo.toml version bump (#793)`)

## TL;DR

After deferring #786 (SemVer MINOR API break) and scope-limiting #789 to the 7 sites in `src/cli/telemetry_cmd.rs`, the cluster fits comfortably as **one patch release (v0.14.11)**. The remaining five items are all P3 tech-debt, behavior-neutral, and total ~6-8 focused hours of work plus gauntlet overhead — well within the cluster pattern.

**The pre-correction composition (6 issues) was NOT patch-eligible** because of #786's `pub fn` removal. After deferring #786, the cluster is clean.

**No priority relabels needed. No dependency changes. No test skips. No CLI/env/config migration. #789's size label** (XS) **is now accurate after scope-limiting** to `telemetry_cmd.rs`. All other labels are confirmed.

## 1. Total effort estimate (post-correction)

| Issue | Claimed size | Realistic | Notes |
|-------|---------------|-----------|-------|
| #785 (docs sweep) | S (half day) | S (2-3 h editorial, 33 sites) | Per-line judgment per #779 precedent |
| #787 (compat shim) | XS (<1 h) | XS (45 min) | Option A: delete + doc-link fix + docs update |
| #788 (CI guard) | S (half day) | S (~3 h: script + Makefile + 43 footnote backfills) | Footnote backfill is ~30 min mechanical |
| #789 (consolidate parse, scope-limited) | XS (<1 h) | XS (~1.5 h; 7 sites, 2 files) | Was S before scope-limit; now accurate as XS |
| #790 (pair_topic) | XS (<1 h) | XS (~15 min, plus the line-161 duplicate) | One const + two call-site swaps |

Sum: **~7-9 hours of focused coding.** Plus gauntlet: T0+T1 <30 s + T2 ~1-5 min + T3 ~6 min + the 8-reviewer F2 pass (~30-60 min, parallel).

The cluster is comfortably one patch release. Splitting is not warranted.

## 2. Per-issue priority verification

| # | Issue | Claimed | Verified | Notes |
|---|-------|---------|----------|-------|
| 785 | TNN-NN sweep | P3 | **P3 ✓** | Behaviour-neutral (33 citations across 11 files, all in doc-comments; verified `rg 'T[0-9]{2}-[0-9]{2}'` returns 0 hits inside string literals, `#[test]` names, or identifiers). |
| 786 | Delete `param_rejections` | P3 | **DEFERRED** | SemVer MINOR break; cannot ship on a patch cluster. See validate-786 report. |
| 787 | Decide `call_uncached_at_temp` | P3 | **P3 ✓** | Function is `pub(crate)` (not pub) at `src/phases/phase.rs:1154`; zero callers verified. Removing or testing is behaviour-neutral. |
| 788 | CHANGELOG guard | P3 | **P3 ✓** | Pure infra hygiene. Bash script added to T0 tier via `make guard-deps`; precedent is `scripts/check-non-interactive-env-guard.sh` added by v0.14.10 cluster (`fab69aa`, `Makefile:117`). |
| 789 | Consolidate RunId parse | P3 | **P3 ✓** | After scope-limiting to 7 sites in `telemetry_cmd.rs`: all 7 sites use the byte-identical format string `format!("invalid run id '{X}': {e}")`. The existing six `cmd.dispatch()` tests assert only `matches!(err, Error::InvalidArgs(_))` — variant check, not text — so consolidating cannot regress test coverage. |
| 790 | `pair_topic` constant | P3 | **P3 ✓** | Already has its own F2 validation report (`docs/cluster-v0.14.11-validation-reports/validate-790-pair-topic.md`). |

**No P1/P2 issues found mislabeled as P3.** All five in-cluster items are real P3 tech-debt.

## 3. Per-issue size verification

| # | Claimed | Verified | Reasoning |
|---|---------|----------|-----------|
| 785 | S | **S ✓** | 33 sites across 11 files, per-line editorial judgment required (not mechanical — `#779` precedent). Issue estimates ~2 h; realistic 2-3 h. The issue's claim matches `#779` which was L (1 week) for 109 sites — so 33/109 ≈ 1/3, and S (half day) is right. |
| 787 | XS | **XS ✓** | Options A (delete + doc-link fix + docs update) and C (test) both ~1 h. Option B is zero work. |
| 788 | S | **S ✓** | ~60-line bash script (well-precedented: 6 existing `check-*.sh` scripts), Makefile target edit, plus footnote backfill. **Footnote backfill is larger than the issue suggests**: 47 version headings vs. 4 footnote entries → ~43 missing footnotes to add. Still fits S — ~30 min extra. |
| 789 | XS | **XS ✓ after scope-limit** | Originally S (8 sites across 3 files). After scope-limiting to the 7 sites in `telemetry_cmd.rs`, the work is ~1.5 h and matches the issue's XS framing. |
| 790 | XS | **XS ✓** | ~15 min as estimated. **Minor expansion**: there's a second hardcoded `"consistency"` at `src/phases/discover_contradict.rs:161` (the empty-findings branch). The `PAIR_TOPIC` const should replace both literals; the F2 report at `validate-790-pair-topic.md` does not flag this, so the implementer may miss it. Add to the commit body as a one-liner. |

## 4. Public API surface

**After deferring #786, no cluster issue touches the public API.** Verified:

- `BreakeredProvider` and `set_param_rejections` survive unchanged (the issue that would have removed them is deferred).
- `call_uncached_at_temp` is `pub(crate)` — its deletion is crate-internal, no external API impact.
- `parse_run_id` is `pub(crate)` — its in-place change (scope-limited to `telemetry_cmd.rs` calling sites) is crate-internal.
- `pair_topic` is a private free function — its replacement with a `const` is crate-internal.
- The new `scripts/check-changelog-release.sh` is a T0 infra script — no runtime API surface.
- `PAIR_TOPIC` is a private const in `src/phases/discover_contradict.rs` — crate-internal.

**Recommendation: PATCH release (v0.14.11).** No semver bump required.

## 5. Dependency changes

None. `grep '^\[' Cargo.toml` shows the same dependency set as `main`; none of the five in-cluster issues reference `Cargo.toml` adds, removes, or version bumps. The `petgraph` optional `dag` feature is untouched.

## 6. Migration concerns

None. No issue introduces or removes:

- Config-file fields (`config.example.toml`, `Config` struct in `src/config/mod.rs`)
- Environment variables
- CLI flags or subcommands (`src/cli/`)
- Storage migrations (`src/storage/migrations/`)

`#788` adds a CI guard script that runs pre-commit and in CI — but does not change runtime behaviour. No operator-facing migration.

## 7. Test count impact

| Issue | Tests added | Tests removed | Tests skipped |
|-------|-------------|---------------|---------------|
| #785 | 0 | 0 | 0 |
| #787 | 0 | 0 | 0 |
| #788 | 0 | 0 | 0 (adds a shell script, not a Rust test) |
| #789 | 0 | 0 | 0 (the existing six `cmd.dispatch()` tests still pass; no test is renamed) |
| #790 | 0 | 0 | 0 |

Total: **0 tests removed, 0 tests skipped.** No `cargo test --skip` change required. No `#[ignore]` added. AGENTS.md "justification in the commit body" rule does not apply.

## 8. Behavioral change audit

| Issue | User-visible string change? | Notes |
|-------|------------------------------|-------|
| #785 | No (comments only) | Citations removed from doc-comments; no string literals, `#[test]` names, or identifiers affected. Verified with `rg '"[^"]*T[0-9]{2}-[0-9]{2}' src/ tests/` (0 hits) and `rg 'fn .*T[0-9]{2}-[0-9]{2}' src/ tests/` (0 hits). |
| #787 | No | `call_uncached_at_temp` is `pub(crate)` and zero callers. Option A also updates `docs/viability/multi-provider-profile.md:62` (rustdoc only — does not reach user output). |
| #788 | No | Pure infra. Adds a bash script that runs pre-commit / in CI; does not affect any user-facing binary behaviour. |
| #789 | No (text-pinned by tests is absent) | All 7 sites use `format!("invalid run id '{X}': {e}")` — byte-identical. The 6 `cmd.dispatch()` tests at `src/cli/telemetry_cmd.rs:2052-2121` assert `matches!(err, Error::InvalidArgs(_))` (variant only, no message text). One outlier (`src/cli/telemetry_cmd.rs:416` — "bad run row: {e}") is for internal SQLite row parsing, not user input, and is left inline per the validate-789 recommendation. |
| #790 | No | `PAIR_TOPIC.to_owned()` produces the same `"consistency"` String as `pair_topic(f)` today. Wire format unchanged. |

**No user-observable string changes.** No `moagan --help` text changes. No CLI help output changes (the `catalog + )` residue that broke `moagan telemetry alerts --help` in v0.14.10 was a different issue — already fixed; not in this cluster).

## 9. Recommendation matrix

| Concern | Risk | Mitigation |
|---------|------|------------|
| `BreakeredProvider.set_param_rejections` removal breaks public API in #786 | **RESOLVED** by deferral | #786 deferred to a future cluster (minor release) |
| Stale "never read" claim in #786 evidence | **RESOLVED** by deferral | N/A |
| #789 under-counts sites (was 17+2, now 7 in scope) | **RESOLVED** by scope-limit | validate-789 prescribes the 7-site scope |
| #789 error-message preservation | **Very Low** | All 7 sites use byte-identical format string; existing tests pin variant only. |
| #790 second hardcoded `"consistency"` at `:161` | **Very Low** | The F2 report at `validate-790-pair-topic.md` should be updated to mention it; the implementer replaces both literals with `PAIR_TOPIC`. |
| #788 Assertion 1 OR-clause flaw | **High (if not caught)** | Tighten per validate-788 finding; the validate-788 report already specifies the fix. |
| #788 footnote backfill size (~43 entries) | **Very Low** | Mechanical, ~25-30 min, fits within S. |
| #785 size underestimate | **Very Low** | `#779` precedent confirms 33 sites in S. |
| Test count impact | **None** | No tests added, removed, or skipped. |
| Dependency change | **None** | Cargo.toml untouched. |
| Migration | **None** | No config / env / CLI changes. |
| Public API break | **None** | #786 deferred; everything else is `pub(crate)` or private. |

## 10. Final recommendation

**Recommendation: PATCH release (v0.14.11) with 5 issues (#785, #787, #788, #789 scope-limited, #790). #786 deferred to a future cluster.**

Cluster fits comfortably (~7-9 h work + gauntlet). No P1/P2 issues mislabeled. No public API breaks. No semver bump required. No split needed.

**Deferral note for #786** (to add to the issue body):

> Deferred from v0.14.11 cluster — the proposed deletion removes `BreakeredProvider::set_param_rejections` (a `pub fn` on `pub struct BreakeredProvider` re-exported through `src/lib.rs:25`'s `pub mod llm;`), which is a SemVer MINOR break. The v0.14.11 cluster is a PATCH release and cannot absorb a MINOR break. The `#[allow(dead_code)]` marker was already removed in commit `fab69aa` (#781 close-out); clippy currently passes because `set_param_rejections` reads the field via `.lock()`. Dispatch path is unchanged (reads the registry-level `ProviderRegistry::param_rejections`). Deferred to a future minor release. See `docs/cluster-v0.14.11-validation-reports/validate-786-delete-param-rejections.md` for the full analysis.

**Two minor cleanup items for the implementer:**

- #786: the issue's "Evidence" section mis-claims the field is never read. Replace with the v0.14.10 F2 wording ("`set_param_rejections` reads it via `self.param_rejections.lock()`") or link the F2 commit message. (Deferral supersedes this.)
- #790: replace BOTH hardcoded `"consistency"` literals (`src/phases/discover_contradict.rs:161` and `:188`) with the new `PAIR_TOPIC` const, not just the function.

## 11. Recommended cluster composition summary

| # | Title | Size | Patch-eligible? | Action |
|---|-------|------|-----------------|--------|
| #785 | docs: sweep 33 TNN-NN citations | S | Yes | Include |
| #786 | refactor(llm): delete BreakeredProvider.param_rejections | XS | **No (MINOR)** | Defer |
| #787 | decision(phases): delete call_uncached_at_temp | XS | Yes | Include (option A) |
| #788 | ci: guard CHANGELOG release bump | S | Yes | Include (with tightened assertion) |
| #789 | refactor(cli): consolidate RunId parse sites | XS | Yes | Include (scope-limited to 7 sites) |
| #790 | refactor(phases): pair_topic ignores argument | XS | Yes | Include (with line 161 update) |

**Cluster total: 5 P3 issues, ~7-9 hours, 100% PATCH-eligible after corrections.**
