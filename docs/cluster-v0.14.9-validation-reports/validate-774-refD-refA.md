# Validation report — issue #774 (`emit_stale_artifact_if_needed` return-type leak)

Validator: `validate-774-refD-refA` subagent
Base commit: `28f2cac` (v0.14.8, on `main`)
Date: 2026-09-06

## TL;DR

The issue's claim is **real**: `emit_stale_artifact_if_needed` at
`src/phases/util.rs:127` returns `Option<StaleArtifactInfo>` but the only
production caller (`read_json`, `src/phases/util.rs:201`) discards the
value. The recommended fix (Option 2 — surface at the `read_json` call
site) **does not actually close the leak**; it only hides it behind a
redundant `tracing::debug!` at the call site. The cleanest minimal
fix is **Option 1 with tests calling the existing private `detect_stale`
directly** — no API leak, no new helper, no test-only scaffolding, and
the existing 4 tests cover exactly that path already. **Verdict:
APPROVE-WITH-MODIFICATIONS** — apply Option 1 + tests-call-`detect_stale`,
not the issue's Option 2.

## Confirmed — the issue is real

Every claim in the issue body matches the source on disk:

| Issue claim | Reality at HEAD | Match |
|---|---|---|
| `src/phases/util.rs:127-145` — `pub(super) fn emit_stale_artifact_if_needed(path: &Path) -> Option<StaleArtifactInfo>` | `src/phases/util.rs:127` — `fn emit_stale_artifact_if_needed(path: &Path) -> Option<StaleArtifactInfo> {` (body to ~line 145) | YES body, NO `pub(super)` — function is fully private (no visibility qualifier at all) |
| Only production caller is `read_json` and it discards the result | `src/phases/util.rs:201` — `emit_stale_artifact_if_needed(path);` (statement, return value dropped) inside `pub fn read_json` at line 190 | YES |
| 3 tests pattern-match on the return value | `stale_artifact_emits_when_artifact_old` (1745), `stale_artifact_silent_when_fresh` (1767), `stale_artifact_respects_env_ttl` (1787) | YES |
| F2-added 4th test pattern-matches | `detect_stale_returns_none_for_missing_file` (1805/1809) | YES |
| TODO comment near `src/phases/util.rs:192` | TODO starts at `src/phases/util.rs:192` (block 192–200) | YES |
| Reference to "WARN→INFO level shift" | Pre-v0.14.8 the old `src/redact/stale_artifact.rs::StaleArtifact::emit` used `tracing::warn!`; v0.14.8 routes through `TelemetryEvent::emit()` (`src/telemetry/event.rs:84-88`) which uses `tracing::info!`. Documented at `CHANGELOG.md:36`. | YES |
| Function was moved from `src/redact/stale_artifact.rs` to `src/phases/util.rs` in v0.14.8 | `CHANGELOG.md:18` documents the move; file deletion verifiable via git log | YES |
| `StaleArtifactInfo` struct (private to `phases/util`) | `src/phases/util.rs:38-49` | YES |

### Minor off-by-N documentation in the issue

The issue cites `src/phases/util.rs:192` as the call site that discards
the return value. Line 192 is actually the **first line of the TODO
comment** (the block runs 192–200); the actual call is at **line 201**:

```
192:    // TODO(orchestrator-followup): consume the `Some(StaleArtifactInfo)`
...
200:    // production caller discards it. Safe to ignore in the same
201:    emit_stale_artifact_if_needed(path);
```

The substance (one production caller discards; in-module tests don't)
is correct. The line citation is the TODO comment start, not the call
site itself — a 9-line off-by-N. Acceptable for a prose issue body but
worth noting in the PR commit message when the TODO is removed.

### Visibility nit

The issue describes the function as `pub(super) fn`; the actual
declaration is `fn` (fully private). The "leak" framing is therefore
slightly weaker than the issue suggests — the symbol is already
file-private, so the leak is into the function's own test module, not
into sibling modules. The hygiene argument still stands (the return
type carries no production value), but `pub(super)` is a documentation
inaccuracy.

## Better alternative — Option 1 + tests-call-`detect_stale`

The issue's Option 2 (surface at `read_json` call site) does **not** fix
the leak:

1. The function still returns `Option<StaleArtifactInfo>` — its
   signature is unchanged, so the test signatures don't change either.
2. `emit_stale_artifact_if_needed` already logs the same fields
   internally at `src/phases/util.rs:137-142` (`tracing::debug!` with
   `path`, `age_secs`, `ttl_secs`) and emits the
   `TelemetryEvent::StaleArtifact` (which itself does `tracing::info!`
   at `src/telemetry/event.rs:87`). Adding a third copy at the
   `read_json` call site is pure redundancy.
3. `read_json` runs the stale check on **every** JSON read (callers
   visible at `src/phases/propose.rs:151`, `src/phases/deliver.rs:44,54,56,230,260`,
   `src/phases/validate.rs:136,147,385`, `src/phases/critique.rs:70`,
   `src/phases/discover_summary.rs:142,256,274,303,366`,
   `src/phases/discover_cluster.rs:121`, and more). The "useful
   post-mortem breadcrumb" framing in the issue would add a fresh
   breadcrumb on every one of these reads — i.e. a per-read log line
   that includes the age of an *unrelated* artefact that happens to
   live alongside the JSON being read. The breadcrumb would be
   confusing, not useful.

**Recommended approach** — Option 1's "drop the return type" goal,
implemented by routing the 4 tests through the existing private
`detect_stale`:

1. `emit_stale_artifact_if_needed(path: &Path) -> Option<StaleArtifactInfo>`
   → `emit_stale_artifact_if_needed(path: &Path)`. The body changes
   minimally: drop the `let info = ...; ...; info` shape, use
   `if let Some(info) = detect_stale(...)` directly. ~5-line body diff.
2. Delete the TODO block at `src/phases/util.rs:192-200`.
3. Rewrite the 4 tests to call `detect_stale(&path, stale_ttl_secs())`
   directly. The test module already has `use super::*` (line 1669) so
   `detect_stale`, `stale_ttl_secs`, and `StaleArtifactInfo` are
   directly in scope — no visibility widening required.
4. Rename the 3 `stale_artifact_*` tests to `detect_stale_*` (the 4th
   one is already named `detect_stale_returns_none_for_missing_file`).
   This documents what each test exercises — today the names lie about
   the function under test.
5. Keep the `stale_artifact_event_serializes_with_kind_tag` wire-form
   test unchanged (it pins `TelemetryEvent::StaleArtifact` JSON, which
   is what `emit_stale_artifact_if_needed` constructs). It is the
   de-facto smoke test for `emit_stale_artifact_if_needed`'s emit path
   without needing a side channel.

Why this beats Option 3 (test-only helper) too:

- Option 3 adds `pub(super) fn detect_stale_for_test(...)` — a second
  function with identical body to `detect_stale`. The cluster already
  added a comment to `src/telemetry/verify.rs::sha256_hex` warning
  against "test-only helper" patterns (see CHANGELOG v0.14.8 §"Fixed —
  Stale dead-code markers", `CHANGELOG.md:41`: "demoted to
  `#[cfg(test)] pub(crate)`; the 3 test sites it served live in the
  same module so no public surface is lost"). The project has already
  concluded that widening visibility for tests is wrong when the tests
  live in the same module — and the `phases/util` tests live in the
  same module, so no widening is needed.

Why this beats Option 1's "trace-capture" sub-variant:

- Tracing capture would mean installing a `tracing_subscriber` layer
  inside `mod tests`. The guard at
  `scripts/check-no-trace-debug-in-mod-tests.sh` (lines 84-88 of the
  script) forbids `tracing::debug!` / `tracing::trace!` inside
  `mod tests` because the process-global `LevelFilter::ERROR`
  installed by `tracing_subscriber::fmt::try_init()` at
  `src/sandbox/process.rs:2535` permanently caches those callsites as
  `Interest::Never`. The `emit_stale_artifact_if_needed` debug emit at
  line 137 would be filtered out, and the `TelemetryEvent::StaleArtifact`
  info-level emit goes through `event.emit()` whose JSON serialization
  uses `unwrap_or_default()` — so a capture test would have to
  JSON-parse the wire form and re-extract the fields it already got
  for free from `detect_stale`. ~50 LOC of test scaffolding for no
  additional coverage.

Concrete test rewrite sketch (single test for brevity):

```rust
#[test]
fn detect_stale_returns_some_when_artifact_old() {
    let _guard = STALE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("old.json");
    std::fs::write(&path, b"{}").unwrap();
    unsafe { std::env::set_var("MOAGAN_STALE_TTL_SECS", "0"); }
    std::thread::sleep(std::time::Duration::from_millis(10));
    let info = detect_stale(&path, stale_ttl_secs());          // <-- changed
    unsafe { std::env::remove_var("MOAGAN_STALE_TTL_SECS"); }
    let info = info.expect("an artefact with age > ttl=0 must surface as stale");
    assert_eq!(info.ttl_secs, 0, "ttl must be propagated from env");
    assert!(
        info.age_secs < 60,
        "fresh-write + 10ms sleep should land well under 60s; got {}",
        info.age_secs
    );
}
```

Function body diff:

```diff
-fn emit_stale_artifact_if_needed(path: &Path) -> Option<StaleArtifactInfo> {
-    let info = detect_stale(path, stale_ttl_secs());
-    if let Some(info) = &info {
+fn emit_stale_artifact_if_needed(path: &Path) {
+    if let Some(info) = detect_stale(path, stale_ttl_secs()) {
         let event = TelemetryEvent::StaleArtifact {
             path: info.path.display().to_string(),
             age_secs: info.age_secs,
             ttl_secs: Some(info.ttl_secs),
             at_unix: now_unix_secs(),
         };
         event.emit();
         tracing::debug!(
             path = %path.display(),
             age_secs = info.age_secs,
             ttl_secs = info.ttl_secs,
             "phases::util::emit_stale_artifact_if_needed: stale artifact emitted"
         );
     }
-    info
 }
```

This is the smallest viable change: 5 LOC deleted from the body,
10 lines deleted from the TODO block, 4 test bodies changed
(`emit_stale_artifact_if_needed(&path)` → `detect_stale(&path, stale_ttl_secs())`),
3 test renames.

## Risk

Low, with one caveat.

**Mechanical risk: low.** The behavior change is:

1. `emit_stale_artifact_if_needed`'s return type goes from
   `Option<StaleArtifactInfo>` to `()`. No production caller uses the
   return (`read_json` at `src/phases/util.rs:201` discards it
   explicitly; nothing else in `src/` or `tests/` references
   `emit_stale_artifact_if_needed` per a repo-wide grep).
2. The 4 tests switch from inspecting the function's return value to
   inspecting `detect_stale`'s return value. `detect_stale` is already
   what `emit_stale_artifact_if_needed` calls internally, so the test
   assertions stay semantically identical. The behavioral assertion
   "for an old file, an event is emitted" is no longer directly tested
   — it's indirectly covered by `stale_artifact_event_serializes_with_kind_tag`
   (which pins the wire form of the event `emit_stale_artifact_if_needed`
   constructs) plus the smoke fact that the function compiles and runs
   without panicking.

**Regression risk worth flagging:**

- **No more end-to-end test that `emit_stale_artifact_if_needed`
  actually emits** for a stale file. If a future refactor removes the
  `event.emit()` call (or routes it through a different sink), the
  test suite won't catch it. Today the same risk exists for
  `tracing::debug!` — both rely on a manual code-review eye.
  Mitigation if you want it: a single integration test in
  `tests/integration_*_tracing.rs` (per-binary isolation per
  `scripts/check-no-trace-debug-in-mod-tests.sh:5-6`) that captures the
  `tracing::info!` event and asserts the JSON contains
  `"kind":"stale_artifact"`. ~30 LOC. Optional — the wire-form test
  covers serialization, and `tracing_subscriber::registry().with(...)`
  capture already exists in the codebase
  (`src/telemetry/redact.rs:97-112`). Recommend adding if the cluster
  is willing to accept an integration-test binary.

**Behavior-change risk for operators:** none. The telemetry event
shape and log levels are unchanged. The only thing that goes away is
a return value no operator ever saw.

**CHANGELOG:** add an entry to the empty `[Unreleased]` section
(`CHANGELOG.md:8`). The v0.14.8 entry at line 50 already mentions
"`StaleArtifactInfo` return type"; a follow-up note explaining that
the return was simplified (and the tests now go through `detect_stale`
directly) keeps the audit trail accurate. The v0.14.8 line should NOT
be edited (per AGENTS.md §"Architectural authority: code wins; ADRs /
CHANGELOG are historical").

## Effort estimate

| Change | LOC | Where |
|---|---|---|
| Function body: drop `Option<StaleArtifactInfo>` return | −5 / +3 | `src/phases/util.rs:127-145` |
| Delete TODO block | −10 | `src/phases/util.rs:192-200` |
| Rewrite 4 tests: `emit_stale_artifact_if_needed(&path)` → `detect_stale(&path, stale_ttl_secs())` | 4 lines, ~16 LOC | `src/phases/util.rs:1745, 1767, 1787, 1809` |
| Rename 3 tests from `stale_artifact_*` to `detect_stale_*` | 3 lines | `src/phases/util.rs:1736, 1759, 1778` |
| Update test doc comments to reference `detect_stale` | ~6 lines | `src/phases/util.rs:1735, 1758, 1777` |
| CHANGELOG `[Unreleased]` entry | ~5 lines | `CHANGELOG.md:8` |
| **Total** | **~25-30 LOC across 2 files** | |

Single commit. Conventional Commits type: `refactor`. Suggested
subject:

```
refactor(phases/util): drop Option<StaleArtifactInfo> return from emit_stale_artifact_if_needed
```

Reference `#774` in the body.

## Verdict

**APPROVE-WITH-MODIFICATIONS.**

The issue correctly identifies the smell but recommends the wrong fix
(Option 2 doesn't actually close the leak; it adds redundant logging).
Apply the issue's *goal* via Option 1 + tests-call-`detect_stale`,
which:

- Removes the `Option<StaleArtifactInfo>` return that no production
  caller uses.
- Has the 4 tests exercise `detect_stale` directly — the function they
  were always implicitly testing, since `emit_stale_artifact_if_needed`
  is a one-line wrapper around `detect_stale(path, stale_ttl_secs())`
  plus a side-effecting emit.
- Adds no test-only helper, no visibility widening, no tracing-subscriber
  scaffolding, no `Mutex<Option<T>>` global (which is on the no-go
  list at `AGENTS.md` §"No-go list").
- Matches the project's existing convention for "tests live in the
  same module, use private helpers directly" — same pattern the v0.14.8
  cluster used for `src/telemetry/verify.rs::sha256_hex` (see
  `CHANGELOG.md:41`).

Apply this. Skip Option 2 and Option 3.

### Issues worth flagging to the cluster author

1. The `pub(super)` in the issue body is wrong; the function is `fn`
   with no visibility qualifier. Minor doc-only nit.
2. Line 192 in the issue body is the TODO comment start, not the call
   site. The call is at line 201. Worth getting right in the PR
   commit-message references so the diff stays greppable.
3. The 4 tests' names currently lie about what they test (the names
   say `stale_artifact_*` but they exercise `detect_stale` via the
   env-var path). Renaming them to `detect_stale_*` while we're in the
   file is a free improvement and worth doing in the same commit.
4. No integration-test coverage for the actual emit-side-effect of
   `emit_stale_artifact_if_needed`. Not strictly required for this
   issue, but worth adding to the cluster's follow-up list if the
   maintainer wants end-to-end assurance.
