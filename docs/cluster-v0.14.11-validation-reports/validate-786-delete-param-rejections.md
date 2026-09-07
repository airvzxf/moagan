# Validation report — issue #786 (delete BreakeredProvider.param_rejections)

> Issue: #786 — `refactor(llm): delete write-only BreakeredProvider.param_rejections field (deferred from #781)`
> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Current HEAD on `main`: `ce524e21ee0028da18384092e60925c69e5172c8` (v0.14.10 release bump)

## TL;DR

The wrapper-level `BreakeredProvider.param_rejections` field is **semantically write-only**: written by both constructors and by `pub fn set_param_rejections`, but no code observes its value for any side effect — the dispatch path reads the registry-level `ProviderRegistry::param_rejections` instead. Behaviourally, option A (delete) is safe: `cargo clippy --all-targets -- -D warnings` already passes on `main` without any marker, because the `set_param_rejections` setter reads `self.param_rejections` to lock it and that single read keeps the `dead_code` lint quiet.

However, the issue body is materially wrong in two places, and the deletion itself is **NOT patch-compatible** because `set_param_rejections` is a `pub fn` on a `pub` struct reachable from `src/lib.rs::llm::provider::BreakeredProvider`. Removing the method is a SemVer MINOR break ("removed pub API"), which conflicts with the cluster's stated intent of staying on a patch release. Recommendation: **defer #786 to a future cluster** (v0.14.12 or a minor release) — the issue body is stale and the scope is more nuanced than the original framing.

## 1. Confirmed — field location, marker status

The issue cites `src/llm/provider.rs:699-701` and shows:

```rust
    #[allow(dead_code)]
    param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,
```

**Field position is correct; the `#[allow(dead_code)]` marker is NOT on the field anymore.** Exact current text:

```
689: /// Self-healing param-rejection table. Wired by
690: /// [`crate::llm/provider::registry_from_config_with_home_and_sink`]
691: /// alongside `max_tokens_table` so future features can route
692: /// per-provider diagnostics through the wrapper. The dispatch
693: /// path consults the registry-level handle on every call rather
694: /// than going through the wrapper, so this field is reserved for
695: /// future per-provider hooks (today it is set but unused at the
696: /// wrapper level — the registry-level table on
697: /// [`ProviderRegistry::param_rejections`] is the runtime source
698: /// of truth).
699:    param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,
700: }
```

`grep -rn 'allow(dead_code)' src/llm/` returns only one match — `src/llm/json_extractor.rs:583` — which is unrelated.

## 2. Why the marker is gone (the issue body is stale)

Commit **`fab69aa` chore(cluster): post-v0.14.9 hygiene cluster (closes #778, #779, #780, #781, #791)** removed the `#[allow(dead_code)]` line as part of the #781 audit. The diff:

```
@@ -696,7 +696,6 @@ pub struct BreakeredProvider {
     /// wrapper level — the registry-level table on
     /// [`ProviderRegistry::param_rejections`] is the runtime source
     /// of truth).
-    #[allow(dead_code)]
     param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,
 }
```

The commit message records the audit's conclusion:

> **#781 — `#[allow(dead_code)]` audit.** Classified by experiment, not by grep: each marker was removed and the verdict taken from whether a lint fired under both `cargo clippy -- -D warnings` and `cargo clippy --all-targets -- -D warnings`. Count went **14 → 6**: 6 markers dropped as redundant, `ContradictionRefinement` deleted, 6 kept with a `// Marker required: …` line recording why.

The marker on `param_rejections` was one of the **6 dropped as redundant**. The empirical reason: `set_param_rejections` at `src/llm/provider.rs:882-884` reads the field via `self.param_rejections.lock()`, and that one read suffices to keep the `dead_code` lint from firing.

**Verified live on `main`** (read-only check, no source modified):

```
$ cargo clippy --lib -- -D warnings
    Checking moagan v0.14.10 (/home/wolf/workspace/projects/moagan)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 33.87s

$ cargo clippy --all-targets -- -D warnings
    Checking moagan v0.14.10 (/home/wolf/workspace/projects/moagan)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 17.97s
```

Both green. The "load-bearing marker" framing in the issue body is **factually wrong as of the current HEAD** — the marker was already removed and clippy still passes.

## 3. Confirmed — zero effective readers

Every occurrence of `param_rejections` in `src/llm/provider.rs` (grouped by which field it names):

| Line | Code | Target field |
|---|---|---|
| 59 | `use super::param_rejections::ParamRejectionsTable;` | import |
| 228 | doc comment | `ProviderRegistry` doc |
| 238 | `pub param_rejections: Option<Arc<ParamRejectionsTable>>,` | `ProviderRegistry` decl |
| 268-269 | `Debug for ProviderRegistry` `if self.param_rejections.is_some()` | registry (Debug) |
| 327 | `param_rejections: None,` | `ProviderRegistry` ctor |
| 376-377 | `pub fn with_param_rejections` — `self.param_rejections = Some(table)` | registry (setter) |
| 386-387 | `pub fn param_rejections(&self)` — `self.param_rejections.as_ref()` | registry (getter) |
| 697 | doc comment | `BreakeredProvider` doc |
| **699** | **`param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,`** | **`BreakeredProvider` decl** |
| 752 | `param_rejections: Mutex::new(None),` | wrapper ctor (`new`) |
| 778 | `param_rejections: Mutex::new(None),` | wrapper ctor (`with_rate_limiter`) |
| **882-884** | **`pub fn set_param_rejections` — `*self.param_rejections.lock() = Some(table)`** | **wrapper (setter — the lone read)** |
| 1486 | `param_rejections: None,` | `ProviderRegistry` ctor in `registry_from_config_with_home_and_sink` |
| 1571, 1587, 1590, 1595 | comments + `registry.with_param_rejections(table)` | registry wiring in same fn |

The single read of the wrapper field is at **line 883** — `self.param_rejections.lock()` inside `set_param_rejections`. That read exists solely so the write can happen; the value being locked is never observed.

Cross-file: no other module reaches into the wrapper. `src/phases/phase.rs` reads `RunContext::param_rejections` (the `PhaseContext` mirror), not the wrapper field. `src/cli/run.rs:495` and `src/cli/discover.rs:810` read `ProviderRegistry::param_rejections()` (the registry accessor), then attach the cloned `Arc` to `RunContext` via `with_param_rejections_opt`. The wrapper field is genuinely terminal — written, never consulted.

The other `Mutex<Option<_>>` fields in `BreakeredProvider` are all live: `rate_limiter` (line 643) is read by `send` at line 954; `saturation_sink` (line 674) is read at lines 969 and 1004; `max_tokens_table` (line 688) is read by `effective_max_tokens`. `param_rejections` is the **only** dead-data-shaped field in the struct. The issue did not miss any neighbours.

## 4. Confirmed — `set_param_rejections` surface

Definition (`src/llm/provider.rs:882`):

```rust
pub fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>) {
    *self.param_rejections.lock() = Some(table);
}
```

Callers (`grep -rn 'set_param_rejections' src/ tests/`):

```
src/llm/provider.rs:882    pub fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>) {
src/llm/provider.rs:1584   wrapped.set_param_rejections(Arc::clone(&table));
```

Exactly one production caller at `:1584`, matching the issue body's claim. Zero test callers.

## 5. Confirmed — `Debug` impl omission

`impl Debug for BreakeredProvider` at `src/llm/provider.rs:716-734` lists six fields:

```
name, model, endpoint, breaker_state,
rate_limiter, rate_limit_max_wait, provider_semaphores
```

`param_rejections` is NOT in the list. Issue body claim is correct.

The `Debug` impl for `ProviderRegistry` at lines 268-269 DOES mention `param_rejections` (printing `"present"` / `"absent"`), but that is the registry-level field, not the wrapper field. Confusing them is a real risk if anyone copy-pastes from `ProviderRegistry`'s pattern.

## 6. API break analysis — this is the blocker

`BreakeredProvider` is reachable as a public type:

- `src/llm/provider.rs:630` — `pub struct BreakeredProvider`
- Module chain: `src/lib.rs:25` `pub mod llm;` → `src/llm/mod.rs` `pub mod provider;` → `pub struct BreakeredProvider`
- `set_param_rejections` is `pub fn` at line 882
- No `#[doc(hidden)]`, no `pub(crate)`, no `#[allow(missing_docs)]` exception

A downstream consumer compiled against the v0.14.x crate can write:

```rust
use moagan::llm::provider::BreakeredProvider;
let bp: Arc<BreakeredProvider> = /* … */;
bp.set_param_rejections(table);
```

**Deleting `set_param_rejections` is a SemVer MINOR break** ("removed pub API"), not a patch-eligible change. This holds even though the only in-tree caller is line 1584 — SemVer is about the surface, not the call graph.

The cluster's stated release target is **v0.14.11 (patch)**. Per `AGENTS.md` §"Commit policy" and the cluster's own framing ("post-v0.14.10 hygiene cluster"), a `pub` method removal forces one of:

1. **Defer #786 to the next minor release** (v0.15.0 or whichever minor branch follows v0.14.11).
2. **Repurpose the cluster to a minor bump.** This requires a release PR with `Cargo.toml` going from `0.14.10` to `0.15.0`, plus the standard release-branch / tag-signature dance documented in `AGENTS.md`.
3. **Keep the field as a permanent reservation** (option C).

Removing only the *field* but keeping `set_param_rejections` as a no-op is also possible but leaves a stale-API landmine (the function will exist forever doing nothing useful, and clippy will not flag it because it touches the field via `.lock()`).

A field-only deletion (drop the field, keep `set_param_rejections` writing into a dead local) does NOT compile — `set_param_rejections` must reference `self.param_rejections.lock()`, so dropping the field also drops the setter. There is no half-measure.

## 7. Test impact

| Test file | Exercises? |
|---|---|
| `tests/integration_param_rejection_self_heal.rs` | Registry-level only (`ParamRejectionsTable::from_home`, `MoaganHome`, `home.param_rejections_path()`). Zero hits on `BreakeredProvider` or `set_param_rejections`. |
| `tests/integration_param_rejection_cascade.rs` | Registry-level only — `build_ctx(...).with_param_rejections(Arc::new(table))` wires via `RunContext`, not the wrapper. |

Source-side unit tests in `src/llm/provider.rs` (lines 2178-3828) build `BreakeredProvider` instances but never call `set_param_rejections` or read the wrapper field — the `MockProvider`-based tests route through the registry.

**Conclusion:** deleting the wrapper field does not break any in-tree test. The cluster's stated validation gate "`tests/integration_param_rejection_self_heal.rs` still green" is correct, but moot because the test never touched the wrapper path.

## 8. Concerns / corrections to the issue body

1. **`#[allow(dead_code)]` is NOT present.** The issue shows the marker at `:699-701`. It was removed in `fab69aa` (#781 close-out). `rg 'allow\(dead_code\)' src/llm/provider.rs` returns nothing.
2. **"Removing the marker makes `cargo clippy -- -D warnings` fail" is wrong as written.** The marker was already removed and `cargo clippy --all-targets -- -D warnings` passes on the current HEAD. The audit in #781 proved this empirically.
3. **The single read in `set_param_rejections` keeps the lint quiet.** This is the structural reason the marker was redundant and why the issue body's "never read" framing is too strong — the field is read by the setter purely to be written back. Net effect: still observationally write-only from any external consumer's perspective, but the rustc lint sees a read and stays silent.
4. **The "three-site refactor touching a `pub` method" deferral is now the load-bearing reason**, not the lint risk. The cluster scope must absorb the SemVer analysis above, or #786 stays parked.

## 9. Recommendation

**Defer #786 out of the v0.14.11 patch cluster.**

Reasoning:

- Behaviour-neutral deletion is provably safe (clippy green, no test touches the wrapper path).
- But it requires removing a `pub` method on a `pub` struct reachable through `moagan::llm::provider`. That is a SemVer MINOR break.
- The cluster's title and `AGENTS.md` guidance both treat v0.14.11 as a patch release. Sneaking a MINOR break into a patch cluster violates the contract.
- The field carries a useful future-reservation doc comment (lines 689-698) that future work can leverage. Deleting it costs ~15 LOC and buys nothing observable.

The issue body must be updated before this ticket is merged in any form — the marker claim is wrong, and the "load-bearing marker" framing would mislead the next reviewer who checks the diff.

## 10. Suggested follow-up issue body

If re-opened, the issue should read:

> Delete the write-only `BreakeredProvider.param_rejections` field, the two constructor initialisers, the `set_param_rejections` setter, and its single call site at `src/llm/provider.rs:1584`. This is a SemVer MINOR break (removes a `pub fn` on a `pub struct` re-exported through `src/lib.rs:25`'s `pub mod llm;`) and must ship on a minor release, not a patch cluster. The marker was already removed in commit `fab69aa` (#781 close-out); clippy currently passes because `set_param_rejections` reads the field via `.lock()`. The dispatch path reads the registry-level `ProviderRegistry::param_rejections` (`src/llm/provider.rs:238`), which survives — wire-level behaviour is unchanged.
