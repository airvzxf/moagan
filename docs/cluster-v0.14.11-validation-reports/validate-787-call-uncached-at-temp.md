# Validation report — issue #787 (decision on call_uncached_at_temp)

> Issue: #787 — `decision(phases): delete or test the uncalled RunContext::call_uncached_at_temp compat shim (deferred from #781)`
> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Current HEAD on `main`: `ce524e21ee0028da18384092e60925c69e5172c8` (`ce524e2 chore(release): v0.14.10 — Cargo.toml version bump (#793)`)

## TL;DR

**Recommend Option A — delete.** The shim has zero callers (production *and* tests), is `pub(crate)` (so deletion is behaviour-neutral outside the crate), and the "compat promise" cited in the issue is documentation of an *existing dispatch surface* rather than a forward-looking contract. Keeping it (Option B) leaves a permanent `#[allow(dead_code)]` marker that is load-bearing for both `cargo clippy -- -D warnings` and `cargo clippy --all-targets -- -D warnings` — a fragile code smell that protects a doc-only promise nobody has verified. Option C spends a permanent test on dead code; the same helper can be re-introduced trivially from the `_for` variant's body when an actual test need surfaces.

A small but real secondary finding: deleting the function breaks a doc-link at `src/phases/phase.rs:1301` (the surviving sibling's doc-comment cross-references the deleted function by `[Self::call_uncached_at_temp]`). One-line fix in the same commit.

## 1. Confirmed — function signature

`src/phases/phase.rs:1131-1196`:

```rust
/// Provider-uncached variant of [`Self::call_with_retry_at_temp`]
/// used by the discovery matrix's retry path (see
/// `discover_matrix::retry_sketch_extraction`). Mirrors
/// [`Self::call_uncached`] but stamps the explicit temperature
/// instead of consulting `resolve_temperature`. The
/// `retry_count` parameter tags the resulting `calls` row so
/// the retry loop's attempt index survives into the JSONL /
/// SQLite `calls.retry_count` column.
///
/// Tanda 04e D-1: the single-provider callers
/// (`DiscoverMatrixPhase` and the coordinator's default-path
/// retry) now go through [`Self::call_uncached_at_temp_for`]
/// which threads the `(section, model)` pair. This
/// single-provider shim is preserved as a thin wrapper around
/// the `_for` variant for any test or integration code that
/// still wants to dispatch against the active context's
/// default pair without threading section/model explicitly.
// Marker required: there are no callers today. The shim is
// documented as a compatibility surface in
// `docs/viability/multi-provider-profile.md`, so withdrawing it
// needs a doc update and an explicit decision to drop that
// promise; deferred to a follow-up.
#[allow(dead_code)]
pub(crate) async fn call_uncached_at_temp(
    &self,
    role: Role,
    system: String,
    user: String,
    started_unix: i64,
    retry_count: u32,
    temperature: f32,
) -> Result<Response> {
```

(Issue cites lines `1145-1152`; the marker comment is at `1148-1152` and the function signature opens on line `1154`. The body runs through line `1196`.)

Confirmed:
- `pub(crate)` (crate-internal), **not** `pub` — the production sibling `call_with_retry_at_temp` at line `1066` is the public seam; the `_at_temp` and `_at_temp_for` helpers are both `pub(crate)`.
- `#[allow(dead_code)]` is on the line immediately above the `fn` keyword (line `1153`).
- The doc-comment is honest: it explicitly says "preserved ... for any test or integration code that still wants to dispatch against the active context's default pair without threading section/model explicitly" — i.e. it self-describes as a compat shorthand, not as a production dispatch path.
- The marker comment on lines `1148-1152` is the load-bearing piece: removing the function or the `#[allow(dead_code)]` attribute fails the clippy gauntlet (per the #781 audit).

## 2. Confirmed — zero callers

`rg '\bcall_uncached_at_temp\b' src/ tests/ docs/`:

```
src/phases/phase.rs:1154         # the definition itself
src/phases/phase.rs:1301         # rustdoc cross-reference inside call_uncached_at_temp_for
docs/viability/multi-provider-profile.md:62   # docs reference
CHANGELOG.md:90                  # changelog entry explaining the deferral
```

No callers in `tests/`; no caller of the shim anywhere in `src/`. The four hits are: (1) the definition, (2) a rustdoc back-pointer, (3) the docs reference that *constitutes* the compat promise, (4) the changelog entry that records *why* it is still here.

`rg '\bcall_uncached_at_temp_for\b' src/ tests/ docs/` (live path):

```
src/phases/discover_matrix.rs:472   # the two production callers
src/discovery/coordinator.rs:1104
src/phases/phase.rs:1308            # the definition
src/phases/phase.rs:1142            # rustdoc cross-reference
src/discovery/coordinator.rs:939    # comment
src/discovery/coordinator.rs:1055   # comment
docs/viability/multi-provider-profile.md:126   # spec entry
CHANGELOG.md:677                    # changelog mention
```

Two production callers, both in the discovery pipeline (coordinator retry path and the legacy `discover_matrix` phase retry path). No test callers.

## 3. Confirmed — docs reference

`docs/viability/multi-provider-profile.md:61-64`:

```markdown
3. The dispatch site (`ctx.call_with_retry_at_temp` /
   `ctx.call_uncached_at_temp`) uses `ctx.default_provider` /
   `ctx.default_model` as the section/model pair — there is no
   per-iteration override path today.
```

This paragraph is *documenting the current dispatch site*, not making a forward-looking promise. The D-1 viability study then introduces `call_with_retry_at_temp_for` / `call_uncached_at_temp_for` precisely because the current dispatch site needs the per-iteration override path the document laments does not exist today. The compat promise is therefore "keep this shorthand callable while we build D-1" — once the `_for` helpers land and the coordinator migrates (which it did in v0.14.x per `CHANGELOG:676-682`), the shorthand's only remaining role is documentation.

The same file at line `126` does list `call_uncached_at_temp_for` as a Phase-2 helper API to add, but does not separately advertise `call_uncached_at_temp` as a contract.

## 4. call_uncached_at_temp_for (the live path)

`src/phases/phase.rs:1298-1348`:

```rust
/// Tanda 04e D-1: provider-uncached sibling of
/// [`Self::call_with_retry_at_temp_for`] used by the
/// discovery coordinator's retry path. Mirrors
/// [`Self::call_uncached_at_temp`] but pins the dispatch to
/// the supplied `(section, model_id)` pair. ...
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_uncached_at_temp_for(
    &self,
    section: &str,
    model_id: &str,
    role: Role,
    system: String,
    user: String,
    started_unix: i64,
    retry_count: u32,
    temperature: f32,
) -> Result<Response> {
    ...
    if let Some(rl) = self.rate_limit_per_role.get(&role) {
        let _wait = rl.acquire().await?;
    }
    self.dispatch_with_governors_for(section, role, async {
        self.dispatch_to_provider_for(section, model_id, req, None, started_unix, retry_count)
            .await
    })
    .await
}
```

### Wire form difference (parameterless vs `_for`)

| Aspect | `call_uncached_at_temp` | `call_uncached_at_temp_for` |
|---|---|---|
| Routing | `self.default_provider` / `self.default_model` | `(section, model_id)` parameters |
| Provider config lookup | `self.config.providers_by_section.get(&self.default_provider)` | `self.config.providers_by_section.get(section)` |
| System prompt prefix | `render_system_prompt_with_prefix(&role, &self.default_model, &system)` | `render_system_prompt_with_prefix(&role, model_id, &system)` |
| `Request.model` | `self.default_model.clone()` | `model_id.to_owned()` |
| AIMD / breaker bucket | `dispatch_with_governors(role, ...)` (per-role only) | `dispatch_with_governors_for(section, role, ...)` (per-section × role) |
| Wire dispatch | `dispatch_to_provider(req, None, ...)` | `dispatch_to_provider_for(section, model_id, req, None, ...)` |

The two are **not** wire-equivalent: the `_for` variant routes through `dispatch_with_governors_for` and `dispatch_to_provider_for`, which key the AIMD governor and the breaker on `(section, role)` rather than on `role` alone. The parameterless form is therefore not just a "default-pair shorthand for the same dispatch" — it picks up a *different* bucket key.

This matters for the decision: a future test that uses the parameterless form would observe different wire-side behaviour than the production discovery path, even if it did not care about section/model routing. The "thin wrapper" framing in the doc-comment is technically accurate (the body is structurally similar) but understates the bucket-key divergence.

### Production callers

- `src/discovery/coordinator.rs:1090-1115` — coordinator's default-path retry branch. The call sits in an `else` arm of `if attempt == 0` (alongside `call_with_retry_at_temp_for` for `attempt == 0`). The pair `(section, model)` comes from the `active_provider_profiles` enumeration (D-1), so the dispatch is pinned to the iteration's `(section, model)`, not the context default.

  ```rust
  let raw = if attempt == 0 {
      ctx.call_with_retry_at_temp_for(
          &section, &model, crate::llm::Role::Sketch,
          system, user, 0, temperature,
      ).await?
  } else {
      ctx.call_uncached_at_temp_for(
          &section, &model, crate::llm::Role::Sketch,
          system, user, started_unix, attempt as u32, temperature,
      ).await?
  };
  ```

- `src/phases/discover_matrix.rs:455-483` — legacy `DiscoverMatrixPhase` retry path; structurally identical to the coordinator branch above, same `attempt == 0` vs `else` shape, same explicit `(section, model)` threading.

Both call sites have migrated to the `_for` variant; no production code threads the parameterless form.

## 5. Option analysis

### Option A — delete

- **Risk:** very low. `pub(crate)` means no external crate can call it; the only callers are inside this crate and there are none. Deletion is byte-identical at the wire level.
- **Behaviour change:** none. No dispatch path uses it; no test calls it.
- **Docs update required:**
  1. `docs/viability/multi-provider-profile.md:61-64` — the paragraph currently names `ctx.call_uncached_at_temp` as the dispatch site. Drop the parenthetical and rephrase; the rest of the sentence (about `default_provider` / `default_model`) still describes the cached sibling, so the rewrite is a single-line deletion plus minor rephrase.
  2. `src/phases/phase.rs:1301` — `call_uncached_at_temp_for`'s doc-comment says "Mirrors `[Self::call_uncached_at_temp]` but pins the dispatch to the supplied `(section, model_id)` pair". The cross-reference target goes away. Rewrite to "Provider-uncached sibling of `call_with_retry_at_temp_for`, used by the discovery coordinator's retry path; pins the dispatch to the supplied `(section, model_id)` pair." That keeps the rustdoc informative without dangling a dead link.
  3. `CHANGELOG.md:90-94` — the entry that records *why* the shim is kept. Move it to the "Removed" subsection under the v0.14.11 cluster with one line summarising the docs cross-ref update.
- **Lint impact:** dropping the `#[allow(dead_code)]` is automatic when the function is deleted. No new `#[allow(...)]` annotations needed. The clippy gauntlet becomes strictly cleaner — removing the marker comment + marker attribute was the load-bearing piece the #781 audit flagged.
- **Effort:** ~30-45 min (one commit, three text edits, gauntlet run).

### Option B — keep as-is

- **Risk:** none now; the marker comment prevents `cargo clippy` from failing.
- **Maintenance burden:** the `#[allow(dead_code)]` on line `1153` is a permanent code smell that future reviewers will (rightly) question. If anyone removes it without reading the marker comment, both `cargo clippy -- -D warnings` and `cargo clippy --all-targets -- -D warnings` fail (#781 audit confirmed). The marker is fragile against well-intentioned cleanup.
- **Semantic value:** zero. No production caller, no test caller, no external consumer (it is `pub(crate)`). The only thing it preserves is the ability to thread one fewer argument from a hypothetical future test — and that test would observe different bucket-key behaviour than production, which is a foot-gun.
- **Effort:** zero. Status quo.

### Option C — keep + test

- **Where the test goes:** `tests/integration_temperature_clamp.rs` is the closest analogue — it exercises `call_with_retry_at_temp` against a `RecordingProvider` registered under `section="recording"` and reads the captured `Request` from a `parking_lot::Mutex<Option<Request>>` slot. The `build_context` helper at lines `116-172` is reusable: it returns `(TempDir, RunContext, Arc<Mutex<Option<Request>>>)` and can be parameterised on `(default_provider, default_model)` so the test sets `default_provider="default-section"` / `default_model="default-model"` (deliberately distinct from the registered `"recording"` section's model) and asserts that `call_uncached_at_temp` produced a `Request.model == "default-model"` and a system prompt prefixed with `"default-model"`.
- **What it asserts:** (a) the parameterless form dispatches against the *context's* default pair (not the registered provider's section), (b) the system-prefix rendering uses the default model, (c) the per-role rate-limit acquire still fires. That proves the shim is a correct thin dispatch over the context default.
- **Marker still required:** yes — `tests/`-only call sites do not appear in `cargo build`'s symbol graph (only in `cargo build --tests` and `cargo build --all-targets`). The plain `cargo build` still sees the function as dead, so `#[allow(dead_code)]` stays. Option C trades "permanent dead function + permanent marker" for "permanent dead function + permanent marker + permanent test". Net cost vs B: +60-90 min of work, plus a permanent test in CI.
- **Effort:** ~60-90 min (one new test in `tests/integration_temperature_clamp.rs` or a new sibling file, plus the gauntlet run).

## 6. Recommendation

**Option A — delete.**

Reasons, condensed:

1. **Zero risk.** `pub(crate)` + zero callers = behaviour-neutral.
2. **Lint signal.** The `#[allow(dead_code)]` on line `1153` is the only such marker in the codebase that protects a *purely documented* promise. Every other marker protects either a `#[cfg(test)]` consumer, a struct-field placeholder, or a serde-driven fixture. Removing this one tightens the lint posture without weakening the others.
3. **Honest docs.** The `multi-provider-profile.md` paragraph at `lines 61-64` currently lists `ctx.call_uncached_at_temp` as the dispatch site, but no production code path uses it — the docs are already drifting from reality. The fix is a one-sentence rephrase that drops the parenthetical reference and keeps the rest.
4. **Cheap to revert.** If a future test genuinely needs the shorthand (e.g. a new phase that only has access to the context default and cannot thread `(section, model)`), the body is ~40 lines and trivial to re-introduce from the `_for` variant as a template. The "preserved compat surface" framing is therefore not load-bearing.
5. **Option C does not pay for itself.** A permanent test on a function with zero production callers is a permanent maintenance cost in exchange for keeping one `#[allow(dead_code)]` marker. The same investment buys meaningful coverage elsewhere (e.g. a real regression test on the `_for` variant's bucket-key wiring).
6. **Option B is the worst.** It is the only option that combines a permanent lint suppression with no consumer at all — a promise nobody has verified, per the issue's own framing.

Pre-merge checklist for the implementing PR:

- [ ] Delete `src/phases/phase.rs:1131-1196` (the doc-comment, marker comment, `#[allow(dead_code)]` attribute, and function body).
- [ ] Rephrase `src/phases/phase.rs:1298-1306` doc-comment to drop the cross-reference to `call_uncached_at_temp`.
- [ ] Update `docs/viability/multi-provider-profile.md:61-64` to remove the parenthetical reference to `ctx.call_uncached_at_temp`.
- [ ] Add a one-line entry under a v0.14.11 cluster in `CHANGELOG.md` noting the shim was withdrawn.
- [ ] `make fmt-check guard-deps lint build test-ci` green on the branch.
- [ ] `rg '\bcall_uncached_at_temp\b' src/ tests/ docs/` returns 0 hits in code (CHANGELOG will still reference it for the audit trail).

## 7. Cross-cutting concerns

Other `#[allow(dead_code)]` markers in `src/`:

| Location | Subject | Status |
|---|---|---|
| `src/phases/phase.rs:779` | `RunContext::heartbeat_spawned` | `pub(crate)`, `#[cfg(test)]` callers in `src/phases/pipe.rs`. Already documented in `CHANGELOG.md:85-87` as the #781 audit finding. |
| `src/cli/doctor.rs:382` | `capabilities_for_kind` | `pub(crate)`, `#[cfg(test)]` callers in the same module. Same #781 entry. |
| `src/cli/probe.rs:527` | `TemperatureProbeResult.model` field | Struct literal initialiser does not count as a read. `CHANGELOG.md:82-84`. |
| `src/cli/probe.rs:613` | `ProbeResult.model` field | Same; `CHANGELOG.md:82-84`. |
| `src/llm/json_extractor.rs:583` | `struct Out` test fixture | Consumed by `extract_and_parse::<Out>` through serde. `CHANGELOG.md:88-89`. |
| `src/phases/phase.rs:1153` | `RunContext::call_uncached_at_temp` | The subject of #787. |

The first five are in a different class from `call_uncached_at_temp`:

- The fields in `probe.rs` are kept because they back a printed report that the per-provider aggregation ignores — the field *will* be read by future reporting work, and removing it requires removing the "echo the pair verbatim" contract on the report.
- `heartbeat_spawned` and `capabilities_for_kind` have `#[cfg(test)]` callers, so they appear dead in the non-test build but not in the test build. The marker is for the production symbol graph, not for tests. They are not orphan shims.
- The `json_extractor.rs` fixture is exercised indirectly through serde-driven deserialisation; `#[allow(dead_code)]` is the lint suppression for a *type* whose fields are only read by reflection-like deserialisation, not a manual accessor.

`call_uncached_at_temp` is the only marker in the codebase whose "compat promise" is *external to the marker* (i.e. lives in `docs/viability/multi-provider-profile.md`, the only multi-provider viability study in the repo) and whose subject has zero callers in any build configuration. It is the only marker that warrants a deliberate decision.

**Recommendation:** address #787 in isolation. Do **not** widen the cluster to the other five markers — they are either (a) intentional struct-field placeholders, (b) `#[cfg(test)]`-consumed, or (c) fixture-driven. Touching them under #787 would dilute the decision and make the PR harder to review.

## 8. Effort estimate

| Option | Code edits | Doc edits | Lint cleanup | New test | Gauntlet | Total |
|---|---|---|---|---|---|---|
| A — delete | 1 (delete fn, drop marker comment + attr) | 2 (`docs/viability/multi-provider-profile.md:61-64`, `src/phases/phase.rs:1301` rustdoc cross-ref) | implicit (one fewer `#[allow(dead_code)]`) | none | 1× `make fmt-check guard-deps lint build test-ci` | ~30-45 min |
| B — keep | 0 | 0 | 0 | 0 | 0 | 0 min (status quo, ongoing maintenance burden) |
| C — keep + test | 0 (function unchanged) | 0 | 0 | 1 (in `tests/integration_temperature_clamp.rs` or sibling file) | 1× gauntlet | ~60-90 min |

## 9. Doc-link breakage alert (cross-cutting concern)

Under option A, the doc-comment of the surviving `call_uncached_at_temp_for` (lines 1298-1306) currently references the deleted function:

```rust
/// Tanda 04e D-1: provider-uncached sibling of
/// [`Self::call_with_retry_at_temp_for`] used by the
/// discovery coordinator's retry path. Mirrors
/// [`Self::call_uncached_at_temp`] but pins the dispatch to
/// the supplied `(section, model_id)` pair. ...
```

Removing `[Self::call_uncached_at_temp]` without rewriting the surrounding prose produces a broken intra-doc link — `cargo doc --no-deps` will flag it (potentially increasing the baseline 80-warning count the cluster invariant pins). The one-line fix: rephrase the "Mirrors ... but pins" sentence to "Provider-uncached sibling of `call_with_retry_at_temp_for`, used by the discovery coordinator's retry path; pins the dispatch to the supplied `(section, model_id)` pair." This is part of the implementation commit, not a separate follow-up.
