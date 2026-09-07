# Validation report — issue #786 (delete `BreakeredProvider.param_rejections`)

> Issue: #786 — `refactor(llm): delete write-only BreakeredProvider.param_rejections field (deferred from #781)`
> Cluster: post-v0.14.11 hygiene (v0.15.0 — MINOR bump, deferred from v0.14.11 patch cluster)
> Current HEAD on `main`: `c3c4c8f6367b31393cb5274055d4aab633bfa897` (v0.14.11 release bump)
> Prior validation: `docs/cluster-v0.14.11-validation-reports/validate-786-delete-param-rejections.md` (written against `ce524e21` / pre-v0.14.11)

## TL;DR

The wrapper-level `BreakeredProvider.param_rejections` field is semantically write-only: written by both constructors and by `pub fn set_param_rejections`, but no code observes its value for any side effect — the dispatch path reads the registry-level `ProviderRegistry::param_rejections` instead. The deletion is safe.

The v0.14.11 cluster deferred #786 because **that cluster was framed as PATCH** and the deletion is a **SemVer MINOR break** (`pub fn` removal on a `pub struct` reachable via `moagan::llm::provider`). The cluster v0.15.0 framing is **MINOR**, which absorbs the break. The deferral reason dissolves under v0.15.0's framing.

The validation report from the v0.14.11 cluster (`docs/cluster-v0.14.11-validation-reports/validate-786-delete-param-rejections.md`) remains accurate on v0.14.11 HEAD (`c3c4c8f`); `src/llm/provider.rs` has not changed since the v0.14.9 cluster (`fab69aa`) removed the `#[allow(dead_code)]` marker. The v0.15.0 cluster reuses that report's findings verbatim and adds independent re-verification below.

## 1. Independent verification — callers trace

`rg -n 'set_param_rejections' src/ tests/ scripts/`:

```
src/llm/provider.rs:882   pub fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>) {
src/llm/provider.rs:1584                      wrapped.set_param_rejections(Arc::clone(&table));
```

Exactly two hits: the definition and the single production caller. **No test, no doc-test, no script, no CLI subcommand** references `set_param_rejections`. The wrapper-level field has no public reader — `rg 'fn .*param_rejections\(' src/llm/provider.rs` returns only the registry-level getter (`pub fn param_rejections(&self) -> Option<&Arc<ParamRejectionsTable>>` at `:386-387`) and the wrapper-level setter (the candidate for removal at `:882`).

Cross-check on `tests/`:

| Test file | Uses setter? | Reads wrapper field? | Notes |
|---|:---:|:---:|---|
| `tests/integration_param_rejection_self_heal.rs` | No | No | Registry-level only (`ParamRejectionsTable::from_home`) |
| `tests/integration_param_rejection_cascade.rs` | No | No | Registry-level only (`RunContext::with_param_rejections`) |
| `tests/integration_circuit_breaker.rs` | No | No | Uses `BreakeredProvider::new(inner, breaker)`, registry-path |
| `tests/integration_pr09_provider_pool.rs` | No | No | Uses `BreakeredProvider::new(...)`, pool semantics |
| `tests/integration_telemetry_saturation.rs` | No | No | Type import only |

**Confidence: HIGH** that deletion is safe at the call-graph level.

## 2. Independent verification — public-API surface

`pub use` statements across the crate (verified live):

| File | Line | Statement |
|---|---|---|
| `src/lib.rs` | 41 | `pub use error::{Error, ExitCode, Result, exit_code};` |
| `src/llm/mod.rs` | 44 | `pub use mock::{MockProvider, MockResponse};` |
| `src/llm/mod.rs` | 45-49 | `pub use models_dev::{ CATALOG_FILE_NAME, ... };` |
| `src/llm/mod.rs` | 50 | `pub use provider::{Provider, ProviderRegistry, registry_from_config};` |
| `src/llm/mod.rs` | 51 | `pub use provider_pool::{ProviderPool, ProviderPoolEntry};` |
| `src/llm/mod.rs` | 52 | `pub use role::Role;` |
| `src/llm/mod.rs` | 53 | `pub use wire::{CallRecord, Request, Response, Usage};` |
| `src/llm/provider.rs` | — | (none) |

**`BreakeredProvider` is NOT in any `pub use` flattening.** It is reachable only via the module path `moagan::llm::provider::BreakeredProvider` (`src/lib.rs:25 pub mod llm;` → `src/llm/mod.rs:32 pub mod provider;`). The closest re-export (`src/llm/mod.rs:50`) exports only `Provider`, `ProviderRegistry`, `registry_from_config` — `BreakeredProvider` is intentionally excluded from that list.

`pub fn set_param_rejections` (`src/llm/provider.rs:882`) is unambiguously public: no `#[doc(hidden)]`, no `pub(crate)`, no attributes of any kind. Removal is a SemVer MINOR break.

## 3. Field visibility vs. setter visibility

- `BreakeredProvider` (`src/llm/provider.rs:630`) is `pub struct`.
- `param_rejections` field (`src/llm/provider.rs:699`) is **private** (no `pub`).
- The `Debug` impl (`src/llm/provider.rs:716-734`) does NOT list `param_rejections` (verified: only `name`, `model`, `endpoint`, `breaker_state`, `rate_limiter`, `rate_limit_max_wait`, `provider_semaphores`).
- `set_param_rejections` (`src/llm/provider.rs:882`) is `pub fn`.

A field-only deletion is **impossible** — the setter references `self.param_rejections.lock()`. Both must go together, which is why the cluster removes both atomically.

## 4. Specific deletion set

The cluster v0.15.0 deletes:

| Site | Lines | Action |
|---|---|---|
| `src/llm/provider.rs:689-699` | Field doc-comment + `param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,` | DELETE |
| `src/llm/provider.rs:752` | `param_rejections: Mutex::new(None),` inside `BreakeredProvider::new` | DELETE |
| `src/llm/provider.rs:778` | `param_rejections: Mutex::new(None),` inside `BreakeredProvider::with_rate_limiter` | DELETE |
| `src/llm/provider.rs:877-884` | Setter doc-comment + `pub fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>)` | DELETE |
| `src/llm/provider.rs:1583-1585` | `for (_key, wrapped) in &wrapped_entries { wrapped.set_param_rejections(Arc::clone(&table)); }` (loop body becomes empty; the whole `for` block goes) | DELETE |

The single call site collapses to: the registry-level `registry = registry.with_param_rejections(table);` line at `:1590` remains, untouched.

## 5. Cross-cluster SemVer framing

The v0.14.11 cluster deferred #786 because PATCH releases cannot remove `pub` API. The cluster v0.15.0 is **MINOR** per SemVer 2.0 §8 ("Minor version Y (x.Y.z | x > 0) MUST be incremented if any public API is removed"). The deferral reason does not transfer.

The wire-level behavior is unchanged: `ProviderRegistry::param_rejections` (`src/llm/provider.rs:238`, accessor at `:386-387`) is the runtime source of truth; the dispatch path reads it on every call. The wrapper-level mirror was reserved for "future per-provider hooks" (per the field doc-comment at `:689-698`) that were never implemented.

## 6. Risks and edge cases

| Risk | Severity | Mitigation |
|---|---|---|
| Downstream consumer of `moagan::llm::provider::BreakeredProvider::set_param_rejections` fails to compile | Inherent to MINOR break | CHANGELOG entry under "Removed public API"; `BREAKING:` footer on the cluster commit; standard SemVer contract. |
| Source-side `MockProvider`-based unit tests in `src/llm/provider.rs:2178-3828` rely on the setter | Confirmed not affected | The tests route through `ProviderRegistry`, not `BreakeredProvider`. Verified: 0 hits on `set_param_rejections` in `src/`. |
| `Debug` impl fallout | None | Field not in Debug field list. |
| Shell-script `grep` guards | None | `scripts/smoke_circuit_breaker.sh:97,101` only structural-greps `pub struct BreakeredProvider` and `impl Provider for BreakeredProvider` — neither is removed. |
| Rustdoc warning delta | Net -3 | Field doc at `:689-699` references private `dispatch_to_provider` (warning #27); setter doc at `:877-884` similarly (warning #47); sibling `with_param_rejections` setter doc (warning #44 — actually a different field, survives). Net: 80 → 77. |

## 7. Validation invariants

- **`rg 'set_param_rejections' src/ tests/` returns 0 hits** (target: 2 → 0).
- **`rg 'param_rejections' src/` shows only registry-level hits** (target: 17 registry + 2 wrapper → 17 registry).
- **`cargo clippy -- -D warnings` and `cargo clippy --all-targets -- -D warnings` clean** (target: no new warnings; the wrapper field's `dead_code` lint silence was driven by `set_param_rejections`'s `.lock()` read — both go together; no new dead code surfaces).
- **`cargo doc --no-deps` warning count 80 → 77** (3 private-link warnings dissolve with the field/setter docs).
- **`make fmt-check guard-deps lint build test-ci` all green.**
- **Smoke gates pass** (`moagan run --mode fast --provider mock:mock-model` produces `final/portfolio.md` and `rankings/ranking.json`).

## 8. Recommendation

Proceed with #786 as part of cluster v0.15.0. The deletion is behavior-neutral, the cluster's MINOR framing is correct, the call-graph analysis is clean, and the public-API surface analysis confirms SemVer MINOR break classification.

Effort: ~30 minutes (3 sites in `src/llm/provider.rs`, ~15 LOC removed, 1 caller change). The cluster's commit-order recommendation (per `validate-cluster-interactions.md`) is **#784 first, then #786** — the docs fix warms the branch before the API-removal commit.

Refs: #781 (the audit that deferred this), #783 (precedent EPIC v0.14.10), #794 (precedent EPIC v0.14.11), `docs/cluster-v0.14.11-validation-reports/validate-786-delete-param-rejections.md` (prior validation, still accurate)
