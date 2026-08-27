# Handoff — `discovery::coordinator` tests leak 15 `/tmp/moagan-*` dirs per run

> **Scope**: this is a focused handoff for **one PR-sized task**: stop the
> 15 tempdir leaks that `cargo test --lib discovery::coordinator::tests`
> leaves behind per invocation. **Pre-existing** — confirmed in
> 2026-08-27 (the prior PR #623 closed the *other* test-leak vector but
> this one was deliberately out of scope). Single-bug report; do not bundle
> unrelated refactors.

---

## 1. TL;DR

After PR #623 landed (`d91bbfc fix(test): stop leaking /tmp/moagan-* in
tests + shrink dev rlibs`), `cargo test --all-targets` still leaves **15
`/tmp/moagan-discovery-coordinator-*` dirs** on disk per run. Same number
on `main` before #623 (verified), so this is a **pre-existing leak** that
#623 did not regress and did not fix.

Root cause: `src/discovery/coordinator.rs::tests` builds a
`DiscoveryCoordinator` inside `with_moagan_home`, then **returns the
coordinator (which owns a `MoaganHome` with an r2d2 SQLite pool) out of
the closure**. When `with_moagan_home`'s `TempDir` drops at the end of the
helper, the SQLite pool still has FDs open to files inside the tempdir,
so `remove_dir_all` returns `ENOTEMPTY` — silently swallowed by
`tempfile::TempDir::drop` (and by the old code's `let _ = ...`).

**Fix**: thread `TempDir` through `with_moagan_home`'s return value so
callers bind it explicitly (same pattern PR #623 used for the SQLite
helpers `unique_db_path` / `unique_regression_path`).

---

## 2. The exact code that leaks

`src/discovery/coordinator.rs`, lines 1397–1447:

```rust
fn new_coordinator(brief: Brief) -> (DiscoveryCoordinator, RunId, PathBuf) {
    new_coordinator_with_mode(brief, Mode::Fast)
}

fn new_coordinator_with_mode(
    brief: Brief,
    mode: Mode,
) -> (DiscoveryCoordinator, RunId, PathBuf) {
    with_moagan_home("discovery-coordinator", |path| {
        EpistemicLegacy::empty()
            .save_to(&path.join("epistemic_legacy.json"))
            .unwrap();
        let run_id = RunId::new();
        let coordinator = DiscoveryCoordinator::new(
            MoaganHome::at(path.to_path_buf()),   // <-- MoaganHome owns a clone of `path`
            run_id,
            Cancel::new(),
            brief,
            "deployment-model:serverless".to_owned(),
            mode,
        );
        (coordinator, run_id, path.to_path_buf())
        // ^ closure returns a tuple that holds:
        //   - `coordinator` → DiscoveryCoordinator → MoaganHome → r2d2 pool → SQLite FDs
        //   - `path`        → clone of the tempdir path
    })
}

fn new_coordinator_with_cancel(brief: Brief) -> (DiscoveryCoordinator, RunId, PathBuf, Cancel) {
    new_coordinator_with_cancel_and_mode(brief, Mode::Fast)
}

fn new_coordinator_with_cancel_and_mode(
    brief: Brief,
    mode: Mode,
) -> (DiscoveryCoordinator, RunId, PathBuf, Cancel) {
    with_moagan_home("discovery-coordinator-cancel", |path| {
        // ... same shape, returns 4-tuple including MoaganHome ...
    })
}
```

### Call-site inventory

13 call sites in the same file, all in `tests` sub-module:

| Line | Pattern |
|---:|---|
| 1509 | `new_coordinator_with_mode(Brief::default(), Mode::Standard)` |
| 1606 | `new_coordinator_with_cancel(brief)` |
| (other 11 lines) | grep `new_coordinator(_with_cancel(_with_mode)?)?\(` to enumerate |

Grep:

```bash
rg -n 'new_coordinator(_with_cancel)?(_with_mode)?\(' src/discovery/coordinator.rs
```

### Test inventory

22 `#[test]` functions total in this file (line range ≈ 1463–2690).
Run any single one in isolation to confirm: **no leak**. Run the full
module: **15 dirs left behind**. The 7 non-leaking tests are the ones
that don't construct a coordinator through these helpers (e.g.
`build_coordinator_matrix_*`, `discovery_iteration_event_*` — they build
the matrix directly without going through `new_coordinator_*`).

---

## 3. Root cause — why `TempDir::drop` fails silently

`tempfile::TempDir::drop`:

```rust
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);  // <-- error swallowed
    }
}
```

When `with_moagan_home("discovery-coordinator", |path| { ... })` returns
to its caller, drop order in the helper's scope is:

```
_restore (env restore)
prev (trivial)
dir (PathBuf drop — trivial)
tmp       <-- TempDir::drop runs HERE, while r2d2 still has FDs open
_guard (reentrant mutex release)
```

`remove_dir_all` on Linux:

- Walks the tree, unlinking entries.
- For files with open FDs in the same process: `unlink` succeeds (file
  stays alive via FD, detached from the namespace).
- For directories containing those unlinked-but-FD-alive files: empty,
  so `rmdir` succeeds.

**Why does it fail here?** Two plausible causes (need verification by
next agent):

1. The r2d2 pool may hold a connection to a SQLite file with WAL + SHM
   sidecars. If `remove_dir_all` walks and tries to unlink `.runs/` or
   `epistemic_legacy.json` or the SQLite WAL/SHM **after** those files
   were already closed, it should still succeed. But if a sidecar is
   held open by a non-SQLite path (e.g. a `BufWriter` flushed but not
   dropped), unlink might fail with `EBUSY` on some filesystems.
2. The `.runs/<run_id>/` subdir may contain a state file
   (`.discovery_state.json` — see line 1395 `const STATE_FILE`) that is
   being held open by an internal writer inside
   `DiscoveryCoordinator`. If the writer wraps a `BufWriter<File>` and
   the `BufWriter` lives inside the coordinator (which lives inside the
   tuple returned to the caller), the `TempDir::drop` fires while the
   `BufWriter` is still alive.

Next agent should **bisect inside the coordinator** to find which field
keeps the FD. Hypothesis to test: `DiscoveryCoordinator::state_writer()`
or whatever wraps `.discovery_state.json` is the culprit.

---

## 4. Reproduction (deterministic)

```bash
# Clean slate
rm -rf /tmp/moagan-* 2>/dev/null

# Single test in isolation — NO leak
cargo test --lib \
  'discovery::coordinator::tests::coordinator_new_stores_brief_and_run_id'
ls /tmp/moagan-* 2>/dev/null | wc -l   # → 0

# Full module — 15 dirs left
rm -rf /tmp/moagan-* 2>/dev/null
cargo test --lib discovery::coordinator::tests
ls /tmp/moagan-* 2>/dev/null | wc -l   # → 15

# Also leaks under --test-threads=1 (not a parallelism issue)
rm -rf /tmp/moagan-* 2>/dev/null
cargo test --lib discovery::coordinator::tests -- --test-threads=1
ls /tmp/moagan-* 2>/dev/null | wc -l   # → 15

# Inspect one leftover to see what survived
ls -la /tmp/moagan-discovery-coordinator-*/.runs/ 2>/dev/null
# → empty .runs/ subdir, no files
```

---

## 5. Proposed fix — three options

### Option A (recommended): thread `TempDir` through `with_moagan_home`

Change `src/test_support.rs::with_moagan_home`:

```rust
pub fn with_moagan_home<F, R>(label: &str, f: F) -> (TempDir, R)
where
    F: FnOnce(&Path) -> R,
{
    let _guard = ENV_LOCK.lock();
    let tmp = tempfile::Builder::new()
        .prefix(&format!("moagan-{label}-"))
        .tempdir()
        .expect("create unique tempdir");
    let dir = tmp.path().to_path_buf();

    // EnvRestore guard (same as before) ...

    let prev = std::env::var_os("MOAGAN_HOME");
    unsafe { std::env::set_var("MOAGAN_HOME", &dir); }
    let _restore = EnvRestore(prev);

    let result = f(&dir);
    drop(_restore);   // restore env explicitly so subsequent code
    drop(tmp.path().to_path_buf());  // (no-op, just for readability)
    (tmp, result)
    // ^ caller now owns `tmp` and must keep it alive for as long as
    //   it holds anything that references the tempdir
}
```

Update every caller from `let x = with_moagan_home("...", |path| { ... });`
to `let (_keep, x) = with_moagan_home("...", |path| { ... });` so the
`TempDir` lives until the test's locals drop.

**Pros**

- Localised to one signature change; matches the pattern PR #623 used
  for `unique_db_path` / `unique_regression_path` (consistency).
- Closes this leak class for *all* future callers, not just the
  coordinator.
- Tests can opt out of the leak by simply binding `_keep`; tests that
  want explicit control can `let (keep, x) = ...; … drop(keep); …`.
- No behaviour change for tests that already work (the ones that don't
  return anything that outlives the closure).

**Cons**

- API break for **all** callers. Grep before starting:

  ```bash
  rg -n 'with_moagan_home\(' src/ tests/
  ```

  Expect ~80 call sites. The bulk (`tests/integration_*.rs` + helpers
  that don't return references to the tempdir) are mechanical
  `s/let (x) = with_moagan_home/let (_keep, x) = with_moagan_home/`.
  The two `new_coordinator_*` helpers in `coordinator.rs` need a
  threaded return like:

  ```rust
  fn new_coordinator_with_mode(
      brief: Brief,
      mode: Mode,
  ) -> (tempfile::TempDir, DiscoveryCoordinator, RunId, PathBuf) {
      with_moagan_home("discovery-coordinator", |path| {
          // ...
          (coordinator, run_id, path.to_path_buf())
      })
  }
  // and each caller:
  let (_keep, coordinator, _run_id, _path) = new_coordinator_with_mode(...);
  ```

- Need to update `with_moagan_home`'s own self-test in
  `src/test_support.rs::tests` (3 tests).

### Option B (smaller scope): restructure only the coordinator helpers

Replace the two `new_coordinator_*` helpers in
`src/discovery/coordinator.rs` with versions that manage the tempdir
themselves (no `with_moagan_home` at all), using the pattern PR #623
introduced for `unique_db_path`:

```rust
fn new_coordinator_with_mode(
    brief: Brief,
    mode: Mode,
) -> (tempfile::TempDir, DiscoveryCoordinator, RunId, PathBuf) {
    let _guard = test_support::ENV_LOCK.lock();
    let tmp = tempfile::Builder::new()
        .prefix("moagan-discovery-coordinator-")
        .tempdir()
        .expect("tmp dir");
    let path = tmp.path().to_path_buf();

    // Save/restore MOAGAN_HOME around the tempdir's lifetime
    let prev = std::env::var_os("MOAGAN_HOME");
    unsafe { std::env::set_var("MOAGAN_HOME", &path); }
    let _restore = test_support::EnvRestoreForTest(prev); // <-- need to expose

    EpistemicLegacy::empty()
        .save_to(&path.join("epistemic_legacy.json"))
        .unwrap();
    let run_id = RunId::new();
    let coordinator = DiscoveryCoordinator::new(
        MoaganHome::at(path.to_path_buf()),
        run_id,
        Cancel::new(),
        brief,
        "deployment-model:serverless".to_owned(),
        mode,
    );
    // Return _restore alongside so caller can drop it explicitly
    // ... or expose EnvRestore from test_support and let it live until tmp
}
```

**Pros**

- Localised to `src/discovery/coordinator.rs`. No API change to
  `with_moagan_home`. Other callers unaffected.

**Cons**

- Duplicates the env-var-lock / save-restore plumbing that
  `with_moagan_home` already does. The DRY violation is real.
- Requires exposing `ENV_LOCK` and `EnvRestore` from
  `src/test_support.rs` for external use (currently they're
  `pub(crate)`-visible inside `test_support.rs`).
- If another file later needs the same pattern, we'll have two ways
  to do it.

### Option C: close the coordinator's pool before returning

Inside `new_coordinator_with_mode`, after building the coordinator:

```rust
// Run a no-op query to ensure the pool has initialised, then drop
// any cached FDs before returning. Not actually possible because the
// pool is owned by `MoaganHome` inside the coordinator — we'd have
// to plumb a `close()` call through `DiscoveryCoordinator` first.
```

**Verdict**: not viable without redesigning `DiscoveryCoordinator`. The
pool is owned by `MoaganHome`, which is consumed by the coordinator.
Closing it requires either:

- Adding a `MoaganHome::close(self)` consuming method (touches
  production code for a test-only concern).
- Adding a `DiscoveryCoordinator::close(self)` consuming method that
  tears down internal state.

Either is an architectural change well outside the scope of this
follow-up.

### Recommendation

**Option A** if you're willing to touch ~80 call sites in a focused
mechanical refactor. **Option B** if you want the change contained to
`coordinator.rs` and don't mind the DRY violation.

---

## 6. Acceptance criteria for this handoff

The fix is **done** when **all** of the following hold:

1. `cargo test --lib discovery::coordinator::tests` leaves **0**
   `/tmp/moagan-*` dirs behind (was 15).
2. `cargo test --all-targets` still reports **0 failed**.
3. `bash scripts/check-no-tempdir-leaks.sh` reports OK (no
   `std::env::temp_dir().*moagan-` patterns).
4. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo build` all clean.
5. The 22 `#[test]` functions in `src/discovery/coordinator.rs` are
   **not deleted** (no `#[ignore]` added, no helper removed) — the
   tests must still run and pass.

Bonus checks (not blocking):

6. `cargo test --lib -- --test-threads=1` also reports 0 leaks (rules
   out parallelism as a confounding factor in future regressions).
7. PR description explains which of the 22 tests were the leaking
   ones (which of them call `new_coordinator_*`) for the reviewer's
   audit trail.

---

## 7. Out of scope (do **not** touch in this PR)

- `src/discovery/coordinator.rs` production code
  (`DiscoveryCoordinator`, `MoaganHome::new`, the pool wiring). The
  leak is a test lifetime issue; production code doesn't construct
  coordinators inside `with_moagan_home`.
- `src/test_support.rs::with_moagan_home` semantics for callers that
  don't return references into the tempdir (Option A's signature
  change is fine because the return is tuple-typed; Option B's
  refactor doesn't touch this file).
- The 19 GB historical leak from before PR #623. PR #623 cleaned up
  the manual `std::env::temp_dir()` pattern; this follow-up cleans
  up the `TempDir` lifetime pattern.
- The `[profile.dev]` size reduction or the `discover.rs::1288`
  Option C refactor (already in PR #623).
- Other integration tests in `tests/integration_*.rs` — they already
  use `tempfile::tempdir()` correctly; nothing to do.

---

## 8. Verification already done by the previous session

To save the next agent duplicate investigation:

- **Counted**: 22 `#[test]` in `src/discovery/coordinator.rs`, 13
  call sites of `new_coordinator_*` helpers, 15 of 22 tests leak (the
  7 non-leaking ones don't use these helpers).
- **Confirmed pre-existing**: ran the same `cargo test` on
  `main` before PR #623's changes → same 15 leaks. So this is **not**
  a regression from #623.
- **Confirmed not parallel-only**: 15 leaks under
  `--test-threads=1` too.
- **Confirmed single-test clean**: any single `new_coordinator_*`
  test in isolation leaves 0 dirs.
- **Inspected leftovers**: each leftover is `moagan-discovery-coordinator-<rand6>/`
  with a single empty `.runs/` subdir. Files were cleaned but the
  outer dir wasn't (because `remove_dir_all` got `ENOTEMPTY` on the
  `.runs` entry which itself couldn't be removed).
- **Read but did not modify**: `src/discovery/coordinator.rs`
  lines 1395–1447 (the four helpers), lines 1463–2690 (the 22
  tests).

---

## 9. References

- **Prior PR**: #623 (`fix(test): stop leaking /tmp/moagan-* in tests
  + shrink dev rlibs`) — landed as squash-merge `d91bbfc` on
  2026-08-27. Established the `(_keep, value) = with_moagan_home(...)`
  pattern for SQLite helpers and added the CI guard. This follow-up
  applies the same pattern to `discovery::coordinator`.
- **CI guard**: `scripts/check-no-tempdir-leaks.sh` (added in #623).
  Catches the `std::env::temp_dir().*moagan-` pattern; does **not**
  catch the `TempDir`-lifetime issue in this report (different bug
  class).
- **Validation tiers**: `docs/validation-tiers.md` — T0/T1/T2 gauntlet
  expectations are unchanged by this follow-up.
- **Commit conventions**: `feat`, `fix`, `refactor`, `docs`, `test`,
  `chore`, `ci`, `build`, `perf`. GPG-signed mandatory (key
  `414687A3CD7E65B9`). Squash-merge to `main`.
- **Related code**:
  - `src/test_support.rs::with_moagan_home` (PR #623's
    `TempDir`-based version).
  - `src/storage/sqlite.rs::unique_db_path` and
    `unique_regression_path` (PR #623's reference pattern: return
    `(TempDir, PathBuf)`).
  - `src/discovery/coordinator.rs::new_coordinator_with_mode` and
    `new_coordinator_with_cancel_and_mode` (the leaky helpers).
  - `src/discovery/coordinator.rs::DiscoveryCoordinator::new`
    (production constructor — receives `MoaganHome` and may clone it
    internally; not to be modified).

---

## 10. PR template suggestion

```text
Title: fix(test): stop /tmp/moagan-discovery-coordinator-* leak in 15 tests

Body:
Follow-up to #623. Same TempDir-lifetime bug class, different code path.
15 of the 22 #[test] in src/discovery/coordinator.rs leak
/tmp/moagan-discovery-coordinator-* dirs per cargo test invocation
because new_coordinator_with_mode / new_coordinator_with_cancel_and_mode
return a DiscoveryCoordinator (which owns an r2d2 pool with open
SQLite FDs) out of the with_moagan_home closure. TempDir::drop fails
silently with ENOTEMPTY once remove_dir_all hits the .runs/ subdir
held open by the coordinator's internal writer.

Fix: <Option A or B from §5 — pick one>.

Verified:
- cargo test --lib discovery::coordinator::tests → 0 leaked dirs
- cargo test --all-targets → 0 failed
- bash scripts/check-no-tempdir-leaks.sh → OK
- cargo fmt --check, cargo clippy -D warnings, cargo build → clean

Closes the "15 leak" item from PR #623's Known follow-up section.
```

---

Signed-off-by: airvzxf
Date: 2026-08-27
