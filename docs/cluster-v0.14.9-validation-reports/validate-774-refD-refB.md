# Validation report — issue #774 (independent angle: refB)

> `refactor(telemetry): tighten emit_stale_artifact_if_needed return type — drop Option<StaleArtifactInfo> leak`
> EPIC: post-v0.14.8 hygiene + bug cluster (#775) · Sub-issue: #774 (P3)
> Pinned: `main` HEAD `28f2cac` (v0.14.8) · Worktree: `validate-774-refD-refB` (local-only)

## TL;DR

1. **VERDICT: APPROVE-WITH-MODIFICATIONS.** The function signature leak is real. Apply **Option 1 + tests-call-`detect_stale`** (drop return to `()`, route the 4 tests through `detect_stale(&path, stale_ttl_secs())`).
2. **Option 2 (surface at call site) is NOT a valid fix** — the issue author recommends it as the smallest change, but Option 2 leaves the `Option<StaleArtifactInfo>` return on the function, only adding a third redundant `tracing::debug!` at the caller. The leak stays.
3. **Option 3 (test-only helper) is over-engineering** — the 4 tests are already in the same module (`pub(super)`-equivalent via `use super::*`); just call `detect_stale` directly.
4. **The 4 tests' names lie about what they test** — they are named `stale_artifact_emits_*` etc. but they all assert on `detect_stale`'s return (not on the emit side-effect). Rename to `detect_stale_*` in the same commit. Free improvement, ~6 char change per name.
5. **Effort:** ~25-30 LOC across 2 files (`src/phases/util.rs` + `CHANGELOG.md`), 1 commit, ~15 min local. Mechanical, low risk.

---

## 1. Confirmed — the leak is real

**Location:** `src/phases/util.rs:127-145`

```rust
fn emit_stale_artifact_if_needed(path: &Path) -> Option<StaleArtifactInfo> {
    let info = detect_stale(path, stale_ttl_secs());
    if let Some(info) = &info {
        let event = TelemetryEvent::StaleArtifact { ... };
        event.emit();
        tracing::debug!(...);
    }
    info
}
```

**Caller:** `src/phases/util.rs:201` — `emit_stale_artifact_if_needed(path);` (return discarded).
**Test consumers:** `src/phases/util.rs:1745, 1767, 1787, 1809` — 4 tests pattern-match on the return.

The issue framing ("leaks into `pub(super)`") is slightly off — the function is `fn` (fully private), not `pub(super)`. The leak is into the same module's `mod tests` only. The hygiene argument still holds; the visibility framing is overstated.

---

## 2. Independent angle — analysis of each option

### Option 1 (drop return, route tests via `detect_stale`)

```rust
fn emit_stale_artifact_if_needed(path: &Path) {
    let info = detect_stale(path, stale_ttl_secs());
    if let Some(info) = info {
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
}
```

Tests become:

```rust
#[test]
fn detect_stale_returns_some_when_artifact_old() {
    let _guard = STALE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("old.json");
    std::fs::write(&path, b"{}").unwrap();
    unsafe { std::env::set_var("MOAGAN_STALE_TTL_SECS", "0"); }
    std::thread::sleep(std::time::Duration::from_millis(10));
    let info = detect_stale(&path, stale_ttl_secs());
    unsafe { std::env::remove_var("MOAGAN_STALE_TTL_SECS"); }
    let info = info.expect("an artefact with age > ttl=0 must surface as stale");
    assert_eq!(info.ttl_secs, 0, "ttl must be propagated from env");
    assert!(info.age_secs < 60, "fresh-write + 10ms sleep should land well under 60s; got {}", info.age_secs);
}
```

**Properties:**
- Diff: function body shrinks by 1 line, return type → `()`, 4 tests renamed + their body changes from `emit_stale_artifact_if_needed(&path)` → `detect_stale(&path, stale_ttl_secs())`. Caller at `:201` is unchanged (already discarding).
- Public API surface: `detect_stale(&Path, u64) -> Option<StaleArtifactInfo>` is `fn` (private) — already accessible to tests via `use super::*` at line 1734.
- Test signal: preserves the exact assertion strength (old/fresh/env-ttl/missing-file branches).
- Behavior change: zero. The `event.emit()` + `tracing::debug!` happen identically inside `emit_stale_artifact_if_needed`. The `TelemetryEvent::StaleArtifact` JSON wire form is unchanged.
- Risk: zero for production; the only risk is test rename + body update, both mechanical.

### Option 2 (surface at call site, leave return type)

The issue author recommends this as the smallest change. **It does NOT close the leak.** The function still returns `Option<StaleArtifactInfo>`; only the call site at `:201` now logs it. The 4 tests still pattern-match on the return. **This is the wrong fix.**

### Option 3 (test-only helper)

The issue author proposes moving the function behind a `#[cfg(test)]` gate and giving tests a different path. **Over-engineering.** The function has a real production caller (`read_json`); making it test-only would require duplicating the logic. Reject.

---

## 3. Independent angle — option 1 vs option 1.5 (rename tests)

Agent-566 noted that the test names lie about what they test. I confirm:

- `stale_artifact_emits_when_artifact_old` (line 1736) — asserts on `info.expect(...)`, not on emit. The `_emits_` is misleading.
- `stale_artifact_silent_when_fresh` (line 1759) — asserts on `info.is_none()`. The `_silent_` is misleading.
- `stale_artifact_respects_env_ttl` (line 1778) — asserts on `info.map(|value| value.ttl_secs) == Some(0)`. The `_respects_` is misleading.
- `detect_stale_returns_none_for_missing_file` (line 1805) — correctly named.

**Rename in the same commit** (Option 1.5):

- `stale_artifact_emits_when_artifact_old` → `detect_stale_returns_some_when_artifact_old`
- `stale_artifact_silent_when_fresh` → `detect_stale_returns_none_when_fresh`
- `stale_artifact_respects_env_ttl` → `detect_stale_propagates_env_ttl`

This is purely cosmetic but eliminates a 1-line future-bug surface (someone renaming `emit_*` could break the test name's contract).

---

## 4. Behavior change for `read_json`

None. The caller at `:201` already discards the return. After the fix:

```rust
emit_stale_artifact_if_needed(path);   // → still discards; still emits event; still logs
let bytes = std::fs::read(path).map_err(...)?;
serde_json::from_slice(&bytes).map_err(...)
```

The wire form `{"kind":"stale_artifact", ...}` is unchanged.

---

## 5. Risk analysis

| Risk | Severity | Mitigation |
|---|---|---|
| Production emit path broken | None — body is unchanged | Diff is return-type-only |
| Test signal weakens | None — same assertions, different fn | Rename tests + body to `detect_stale(&path, stale_ttl_secs())` |
| CHANGELOG entry missing | Cosmetic | Add 2 lines to `[Unreleased]` |
| Future contributor confused about test rename | Low | Commit message explains the rename; CHANGELOG entry references both names |
| `#[allow(dead_code)]` lint trips on `detect_stale` after the move | Trivial | `detect_stale` is `fn` (private); still called by `emit_stale_artifact_if_needed` and tests |

---

## 6. CHANGELOG entry

In `CHANGELOG.md` `[Unreleased]` (line 8):

```markdown
- refactor(phases/util): `emit_stale_artifact_if_needed` returns `()`
  instead of `Option<StaleArtifactInfo>`. The return value was only
  consumed by the in-module tests; the production caller at `read_json`
  discarded it. The 4 affected tests now call `detect_stale` directly
  and are renamed to match the new contract. No behavior change.
  Closes #774.
```

---

## 7. Suggested commit message

```
refactor(phases/util): drop Option<StaleArtifactInfo> return from emit_stale_artifact_if_needed

`emit_stale_artifact_if_needed(path)` at src/phases/util.rs:127 returned
`Option<StaleArtifactInfo>`, but the only production caller (read_json
at :201) discarded the value. The return type existed solely so the
4 in-module tests at :1736/:1759/:1778/:1805 could assert on what was
emitted. This is the hygiene pass that #770/#771 deferred with the
`TODO(orchestrator-followup)` comment at :192-200.

Convert the function to return `()`. The 4 affected tests now call
`detect_stale(&path, stale_ttl_secs())` directly — same assertions,
cleaner contract — and are renamed to reflect that they test
`detect_stale`, not the emit helper:

- stale_artifact_emits_when_artifact_old → detect_stale_returns_some_when_artifact_old
- stale_artifact_silent_when_fresh       → detect_stale_returns_none_when_fresh
- stale_artifact_respects_env_ttl        → detect_stale_propagates_env_ttl
- detect_stale_returns_none_for_missing_file  (already correctly named)

Behavior change: zero. The `TelemetryEvent::StaleArtifact` wire form
emitted via `event.emit()` and the `tracing::debug!` breadcrumb are
identical. The `read_json` call site is unchanged.

Refs #774 (EPIC #775)
```

---

## 8. Effort estimate

**One commit.** ~25-30 LOC across 2 files.

- `src/phases/util.rs`: function signature (1 line); 4 test renames + 4 test bodies (~24 lines net change).
- `CHANGELOG.md`: ~6 lines under `[Unreleased]`.

No new deps, no API surface change beyond the private `fn`, no schema change.

T1 + T2 green before push. CI green on `main` after squash-merge.

---

## 9. Verdict

**APPROVE-WITH-MODIFICATIONS.**

Apply **Option 1 + tests-call-`detect_stale`** (drop return type + rename 4 tests + route tests through `detect_stale`). Skip Option 2 (it doesn't close the leak) and Option 3 (over-engineering).

---

## Appendix — citations

| Claim | File | Lines |
|---|---|---|
| Function definition | `src/phases/util.rs` | 127–145 |
| Production caller | `src/phases/util.rs` | 201 |
| `detect_stale` (private, accessible to tests) | `src/phases/util.rs` | 62–125 |
| Test 1 (`_emits_when_old`) | `src/phases/util.rs` | 1736–1756 |
| Test 2 (`_silent_when_fresh`) | `src/phases/util.rs` | 1759–1776 |
| Test 3 (`_respects_env_ttl`) | `src/phases/util.rs` | 1778–1803 |
| Test 4 (already correctly named) | `src/phases/util.rs` | 1805–1811 |
| Wire-form test (unchanged) | `src/phases/util.rs` | 1813+ |
| TODO(orchestrator-followup) comment | `src/phases/util.rs` | 192–200 |
| CHANGELOG `[Unreleased]` (currently empty) | `CHANGELOG.md` | 8 |
| v0.14.8 cluster release body | `28f2cac` | — |

---

## Summary for parent agent

- **Report path:** `/home/wolf/workspace/projects/moagan/.worktrees/validate-774-refD-refB/REPORT-774-refB.md` (this file).
- **TL;DR (5 lines):**
  1. The issue is real; apply Option 1 (drop return) + tests-call-`detect_stale` (route 4 tests through the private `detect_stale`).
  2. The issue's recommended Option 2 does NOT close the leak — the function still returns `Option<StaleArtifactInfo>`. Reject.
  3. The 4 test names lie about what they test (they test `detect_stale`, not emit). Rename them in the same commit.
  4. Behavior change for production: zero. The wire form is unchanged.
  5. Verdict: APPROVE-WITH-MODIFICATIONS. ~25-30 LOC, 1 commit, ~15 min local.
- **Concerns to flag:** the issue author's framing of "leak" is correct in spirit but wrong in visibility (`fn` is private, not `pub(super)`); and their Option 2 does not actually close the leak — the cluster author must pick Option 1 explicitly.
