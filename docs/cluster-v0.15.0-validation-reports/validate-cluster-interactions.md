# Validation report — cluster v0.15.0 interactions

> Cluster: post-v0.14.11 hygiene (v0.15.0 — MINOR bump)
> Sub-issues: #786 (delete `BreakeredProvider.param_rejections` field + `set_param_rejections` setter) + #784 (4 functionally degraded + 1 broken rustdoc link from #773 residue)
> Validation: 8-reviewer F1 swarm

## TL;DR

**Safe to proceed — issues are completely disjoint.** #786 touches only `src/llm/provider.rs` (a public `pub fn` removal); #784 touches 4 unrelated files with zero `pub` API impact. No shared code paths, no shared tests, no shared doc-link targets. The cluster's MINOR framing (v0.14.11 → v0.15.0) is required to absorb #786's SemVer break; #784 is docs-only and would be patch-eligible on its own. **Recommended commit order: #784 → #786** (smaller first, larger last — matches v0.14.11 cluster pattern).

## 1. File-by-file diff

### #786 — `src/llm/provider.rs` only

| Lines | Current content | Action |
|---|---|---|
| 689-699 | Field doc-comment + `param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,` | DELETE |
| 752 | `param_rejections: Mutex::new(None),` inside `BreakeredProvider::new` | DELETE |
| 778 | `param_rejections: Mutex::new(None),` inside `BreakeredProvider::with_rate_limiter` | DELETE |
| 877-884 | Setter doc-comment + `pub fn set_param_rejections(...)` | DELETE |
| 1583-1585 | `for (_key, wrapped) in &wrapped_entries { wrapped.set_param_rejections(Arc::clone(&table)); }` (loop body becomes empty) | DELETE (entire `for` block) |

Plus: `Cargo.toml` version bump `0.14.11 → 0.15.0` (MINOR — in the release commit, not the cluster commit).

### #784 — 4 files, all cosmetic / docs-only

| File | Line | Current content | Action |
|---|---|---|---|
| `src/error/mod.rs` | 290 | `` [`                                `](../                                ) `` — empty backtick text + empty path | REWRITE to working intra-doc link `[`research::pdf`]` OR drop the parenthetical "see … for the install hint" |
| `src/discovery/contradiction.rs` | 81 | `` `            §D.x contradiction` `` — whitespace residue in backticked text | SQUASH to prose: drop the backticked citation, restate the empirical fact inline |
| `src/telemetry/mod.rs` | 295 | `// Spec      declares `gz` as the default compression…` — multi-space residue in `//` comment | RESTATE inline: drop "Spec" + 6 spaces, fold the `gz` fact into the comment opening |
| `src/redact/patterns.rs` | 232 | `// Categorised redaction (                  )` — empty parenthetical header | DROP the parenthetical: `// Categorised redaction` |

**File-set intersection: EMPTY.** `#786 ∩ #784 = ∅`.

## 2. Test coverage analysis

### Test files referencing `BreakeredProvider` / `param_rejections` / `set_param_rejections`

| Test file | Hits | Uses setter? | Reads wrapper field? | Touches any #784 file? |
|---|---:|:---:|:---:|:---:|
| `tests/integration_circuit_breaker.rs` | 13 | No | No | No |
| `tests/integration_param_rejection_cascade.rs` | 12 | No (uses `RunContext::with_param_rejections` — registry-level) | No | No |
| `tests/integration_telemetry_saturation.rs` | 7 | No | No | No |
| `tests/integration_param_rejection_self_heal.rs` | 4 | No | No | No |
| `tests/integration_pr09_provider_pool.rs` | 3 | No | No | No |

`rg -n 'set_param_rejections' tests/` returns **zero hits.** `rg -n 'param_rejections' tests/` shows registry-level wiring only.

### Test files touching the 4 #784 sites

- `src/redact/patterns.rs:232` — `tests/integration_phase_l.rs:8` and `tests/integration_phase_k.rs:32` import `moagan::redact::patterns::{PATTERNS, PatternKind, substitute}`. **None** read line 232 (a `//` banner comment above `pub fn substitute`).
- `src/discovery/contradiction.rs:81` — No test file imports anything from `src/discovery/contradiction.rs`; the helper is consumed only via `moagan::domain::Contradiction`.
- `src/error/mod.rs:290` — Test files import `moagan::error::{Error, Result, IoError, ExitCode}` as types only (verified across `tests/integration_*.rs`). Line 290 is a doc-comment.
- `src/telemetry/mod.rs:295` — Tests import `moagan::telemetry::{Telemetry, WarningContext}` as types. Line 295 is a `//` comment inside `Telemetry::open`.

**Conclusion: zero tests touch any line either issue modifies.** Both issues are safe from a test-coverage perspective.

## 3. Public API surface

### #786 — SemVer MINOR break (load-bearing)

- `pub fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>)` on `pub struct BreakeredProvider` (`src/llm/provider.rs:882`) is removed.
- Module path: `moagan::llm::provider::BreakeredProvider::set_param_rejections`.
- Cluster framing as **v0.15.0 (MINOR)** is correct. A patch bump (v0.14.12) would violate SemVer.

### #784 — Zero `pub` API impact

All four fixes touch only:

- Doc-comment text (intra-doc links, prose)
- `//` line comments
- A backtick-quoted prose reference inside a `///` doc-comment

None alters any function signature, struct field, type alias, or `pub` re-export. **PATCH-compatible** in isolation — the cluster is MINOR only because #786 forces it.

## 4. Commit-order recommendation

**Order: #784 → #786** (2 commits, single cluster PR following the v0.14.11 / `4ab8caf` precedent).

**Rationale:**

1. **#784 lands first.** Smaller, docs-only, zero runtime risk, zero public API impact, easy to verify (`fmt-check` + `clippy` both already green; rustdoc warning count stays at or below 80). Warms the cluster branch with a no-risk commit, mirroring the v0.14.11 cluster pattern (which was `#790 → #785 → #787 → #789 → #788` — smallest-first).
2. **#786 lands second.** The bigger behavioral change (code deletion + `pub fn` removal). Lands second so reviewers see "docs-cleanup → API-removal" in chronological order — easier bisect if a regression surfaces downstream.

**Recommended commit subjects:**

- `docs: clean 4 #773-deferred residue sites (error, contradiction, telemetry, redact) (closes #784)`
- `refactor(llm): delete write-only BreakeredProvider.param_rejections (closes #786, MINOR API break)`

**Cluster PR body** (template, mirrors `4ab8caf`):

```
chore(cluster): v0.15.0 cluster — BreakeredProvider.param_rejections removal + #773 whitespace residue repairs (closes #784, #786)

Cluster validation: 8-reviewer swarm. #784 = 4 docs-only residue fixes
(deferred from #773); #786 = MINOR API break (deferred from v0.14.11
because that cluster was PATCH-framed) — now lands as v0.15.0 because
removing BreakeredProvider::set_param_rejections is a SemVer MINOR break.

Wire-level behavior is unchanged: ProviderRegistry::param_rejections
(the runtime source of truth) survives; only the wrapper-level mirror
field goes.
```

## 5. Validation gauntlet — does anything beyond T2 need to run?

Standard T2: `make fmt-check guard-deps lint build test-ci`. Both issues should pass cleanly:

- **`make fmt-check`** — #786 deletes lines (no reformat); #784 rewrites prose (no alignment traps because none of the 5 sites are inside doc-comment tables). Both pass.
- **`make guard-deps`** — `scripts/check-no-forbidden-crates.sh` is dependency-only; both issues touch no `Cargo.toml` deps. The CHANGELOG release guard from #788 will not fire on the cluster commit (the version bump happens in the release commit). Pass.
- **`make lint`** (`cargo clippy --all-targets -- -D warnings`) — `src/llm/provider.rs`'s wrapper field is currently kept alive by `set_param_rejections`'s `self.param_rejections.lock()` read. Deleting both removes the only read path; the dispatch path reads `ProviderRegistry::param_rejections` (the registry field, line 238, which survives). **No new clippy warnings.**
- **`make build`** — Zero in-tree callers of `set_param_rejections` (verified: only `:882` and `:1584`). Clean build.
- **`make test-ci`** — Registry-level tests (`integration_param_rejection_*.rs`) exercise `RunContext::with_param_rejections`, not the wrapper field. They pass.

### Extra step — `cargo doc --no-deps` warning baseline

**Baseline: 80 warnings** (live-verified via `cargo doc --no-deps --lib 2>&1 | grep -c 'warning:'` on `c3c4c8f`).

| Site | Effect on rustdoc warning count |
|---|---|
| #786 `src/llm/provider.rs:689-699` (field doc) | Doc-block deletes with the field; intra-doc targets `[crate::llm/provider::registry_from_config_with_home_and_sink]` and `[ProviderRegistry::param_rejections]` both SURVIVE — no orphan links. **-1** (private-link warning at `:230` dissolves). |
| #786 `src/llm/provider.rs:877-884` (setter doc) | Doc-block deletes with the method; no external references. **-2** (private-link warnings at `:672` and `:230` dissolve). |
| #784 `src/error/mod.rs:290` | Current `[` `](../                                )` does NOT generate a warning (empty backtick text → rustdoc treats as self-reference / ignored). Fix is hygiene; warning count unchanged OR -1 if the prose-only alternative is chosen. **±0 to -1** |
| #784 `src/discovery/contradiction.rs:81` | Backtick-quoted text, not a rustdoc intra-doc link. No warning to remove or create. **±0** |
| #784 `src/telemetry/mod.rs:295` | `//` comment, not rustdoc. **±0** |
| #784 `src/redact/patterns.rs:232` | `//` comment, not rustdoc. **±0** |

**Net: `cargo doc --no-deps` must report ≤ 80 warnings after both issues land** (predicted: 77-79). Pin this in the cluster PR's validation section so reviewers can spot-check.

## 6. Cascade risks

### Risk 1: `src/telemetry/mod.rs` doc-references to `BreakeredProvider` (3 hits)

Lines 954, 979, 1011 each reference `BreakeredProvider`:

- L954: `[crate::llm::provider::BreakeredProvider] hook.` — struct survives #786. **No break.**
- L979: `[crate::llm::provider::BreakeredProvider] hook.` — struct survives. **No break.**
- L1011: `[crate::llm::provider::BreakeredProvider::send]` — `send` is a method on `impl Provider for BreakeredProvider`, survives. **No break.** (Pre-existing warning at L1011 about "no item named `send`" is independent of #786.)

### Risk 2: `Debug for BreakeredProvider` (lines 716-734)

Verified: `param_rejections` is NOT in the Debug field list. Removing the field does not require any Debug change. **No break.**

### Risk 3: Cross-cluster overlap with prior validation report

The v0.14.11 validation report §8.1 explicitly noted: "#785 leaves #784's whitespace residue on lines NOT in the 11 TNN-NN files … #784 remains the responsible ticket." This v0.15.0 cluster correctly absorbs that debt by carrying #784 forward. **No duplication.**

### Risk 4: Shell-script grep guards

- `scripts/smoke_discovery.sh:271,274` greps `src/discovery/contradiction.rs` for `pub fn top_pairs` and `pub fn severity_rank` (lines 52, 65). Line 81 is untouched.
- `scripts/smoke_phase_k.sh:159-168` greps `src/redact/patterns.rs` for `pub enum PatternKind`, `SkCpApiKey`, `BearerHeader`, `AnthropicApiKey`, `pub fn substitute`, `REDACTED:api_key:sk-cp`, `Bearer ***REDACTED***`. None touch line 232.

**No break.**

### Risk 5: CHANGELOG.md cross-cutting concerns

Both issues warrant `[Unreleased]` entries; the cluster PR must add them. The `#786` entry should explicitly call out the SemVer MINOR break (and link to `validate-786-delete-param-rejections.md` for the rationale); the `#784` entry is a 4-line docs-cleanup note. No collision.

## 7. Validation summary

| Aspect | #786 | #784 | Conflict? |
|---|---|---|---|
| Files touched | `src/llm/provider.rs` only | 4 different files | **No** |
| Public API change | Yes — `pub fn set_param_rejections` removal | None | No |
| SemVer | **MINOR** (forces v0.15.0) | None (PATCH-eligible in isolation) | No |
| Tests touched | Zero | Zero | **No** |
| Doc-link targets | Both intra-doc links in field doc survive | One rustdoc link fixes; rest are prose/comments | No |
| rustdoc warning delta | -3 (private-link warnings dissolve) | 0 to -1 | **No** |
| Clippy impact | None (no new dead code) | None | No |
| Cascade into other files | None (verified `src/telemetry/mod.rs` references survive) | None | No |

**Confidence level: HIGH.** The cluster is mechanically safe; the only structural requirement is the MINOR bump framing for #786, which EPIC #797 correctly specifies.

Refs: #779 (prose-rewrite precedent), #783 (precedent EPIC v0.14.10), #785 (cluster v0.14.11 cite-sweep pattern), #788 (CHANGELOG guard), #794 (precedent EPIC v0.14.11)
