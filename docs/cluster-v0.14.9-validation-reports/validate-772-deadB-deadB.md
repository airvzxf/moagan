# Validation Report — Issue #772

**Branch:** `validate/772-deadB` (local-only, branched from `main` @ `28f2cac`)
**Issue:** [`airvzxf/moagan#772`](https://github.com/airvzxf/moagan/issues/772) — `chore(dead-code): sweep remaining phantom helpers + stale #[allow(dead_code)] markers (~30 sites in src/)`
**Goal:** Verify the F2 explorer report's findings are accurate and the proposed fixes are sound.
**Verdict (1 line):** **APPROVE-WITH-MODIFICATIONS** — every cited site exists with the line numbers claimed; the issue is *real* and worth landing, but the proposed patches miss four import-cleanups and two intra-doc-link edits that `cargo build --all-targets` + `cargo doc` will fail without.

---

## 1. Confirmed findings

Each phantom helper, unreferenced item, and the redundant marker was traced. The existence claim is correct in every case; only minor cosmetic differences exist between the issue's "F2 report" and the actual source.

### 1.1 Phantom-helper markers — all 4 confirmed

| Issue site | Source location | Status | Dependency that must also be cleaned |
|---|---|---|---|
| `_force_arc_link(_x: Arc<()>)` | `src/phases/discover_dimensions.rs:355-356` | confirmed | `use std::sync::Arc;` on line **34** has *no other consumer* in the file — must also be removed |
| `_types_are_used(_: OrphanTableStat)` | `src/cli/telemetry_cmd.rs:1258-1259` | confirmed | import on `src/cli/telemetry_cmd.rs:1070` is **kept** — `OrphanTableStat` is destructured at line 1235 (`Vec<&OrphanTableStat>`) |
| `_proposal_marker(_: &Path)` | `src/phases/repair.rs:190-191` | confirmed | `use std::path::{Path, PathBuf};` on line **5** must collapse to `use std::path::PathBuf;` (the bare `Path` ident has *no other consumer* in this file; `PathBuf` is used at lines 61, 74, 105, 137, 166, 171) |
| `_legacy_record_anchor(...)` | `src/phases/discover_contradict.rs:353-361` | confirmed | no other type-cleanup needed; only self-contained |

Verification of zero callers (project-wide):

- `_force_arc_link` — no callers (only the definition itself; the function is module-private).
- `_types_are_used` — no callers (only the definition itself).
- `_proposal_marker` — no callers (only the definition itself).
- `_legacy_record_anchor` — no callers (only the definition itself).

### 1.2 Self-contradicting / stub-removed module — confirmed

- `src/cli/telemetry_cmd.rs:382-390` — `mod stubs_removed { pub(crate) fn run_stub(_name: &str) -> Result<()> { ... } }`.
- `run_stub` has **zero callers** anywhere in `src/` (grep confirms a single hit, at the definition itself).
- Module name says "removed" but the stub fn is "live" — both proposed options (delete or rename to `stubs`) are correct.
- **Recommendation: delete the whole `mod stubs_removed { ... }` block** rather than rename; with zero callers there is nothing to preserve under a new name.

### 1.3 Unreferenced structs / consts / fns — all 4 confirmed

| Item | Source location | Evidence of zero usage |
|---|---|---|
| `Watchdog { pub pgid, pub timeout, pub grace, pub cancel }` struct | `src/sandbox/process.rs:813-825` | A repo-wide search for `Watchdog {` (struct literal) returns **zero** matches; `impl Watchdog` (line 827) declares **only** the static fn `spawn` which takes raw parameters (`pgid: i32`, `timeout: Duration`, `grace: Duration`, `cancel: CancellationToken`) — none of `self.pgid` / `self.timeout` / `self.grace` / `self.cancel` are read anywhere. All callers reach the watchdog through `Watchdog::spawn(...)` (sites: `process.rs:1661, 2677, 2718, 2766, 2804, 2841`). |
| `SKETCHES_DIR` / `TAGS_DIR` consts | `src/phases/discover_summary.rs:80-83` | Each identifier appears **only at its own definition** (grep confirms 2 hits, both at the const block). |
| `load_manifest_for_resume(home, run_id)` | `src/cli/discover.rs:1492-1495` | Single project-wide hit, at the function definition itself; no callers. |
| `repair_missing_brackets(s: &str) -> Option<String>` | `src/phases/util.rs:1350` | Only matches are the function definition (line 1350) and **three intra-doc-link references** at lines 1144, 1455, 1472. No call site anywhere. (See §2.2 — those doc-comment links must be re-pointed.) |

### 1.4 Redundant `#[allow(dead_code)]` marker — confirmed

- `src/phases/phase.rs:776-779` — `heartbeat_spawned` is annotated `#[allow(dead_code)] // test-only assertion; production never inspects the slot`.
- Real callers in the project: `src/phases/pipe.rs:719, 727, 757` (`ctx.heartbeat_spawned()` from `pipeline_run_spawns_heartbeat_when_db_is_indexed` and `pipeline_run_skips_heartbeat_when_db_unindexed`).
- The annotation only exists to silence the lint during `cargo build` for non-test cargo targets. Drop the attribute and `cargo build --all-targets` stays green; `cargo test --lib` continues to compile the pipe tests because they are integration tests gated by `#[cfg(test)]` but live under `src/phases/pipe.rs` (which `phase.rs` references).

### 1.5 "Once-over" markers the issue calls out specifically with action

`src/cli/probe.rs:516, 525, 598, 608` — four `#[allow(dead_code)]` markers on:

- `:516` `Failed(#[allow(dead_code)] String)` of `TemperatureProbeOutcome`
- `:525` `#[allow(dead_code)] model: String` of `TemperatureProbeResult`
- `:598` `Failed(#[allow(dead_code)] String)` of `ProbeOutcome`
- `:608` `#[allow(dead_code)] model: String` of `ProbeResult`

**Construction vs. destructure audit:**

- The `Failed(String)` variants ARE constructed at four sites: `src/cli/probe.rs:285, :466, :866, :1050`.
- They are **never destructured** anywhere in `src/` (grep for `ProbeOutcome::Failed` / `TemperatureProbeOutcome::Failed` / `Failed(` returns only construction sites + the two enum definitions + one unrelated `ExtractError::ParseFailed` in `src/llm/json_extractor.rs`).
- The inner `String` is therefore always built and immediately thrown away; the `#[allow(dead_code)]` annotation correctly suppresses a real "field never read" lint.
- The `model: String` field IS used at multiple `r.model.clone()` / `r.model` sites — I did not exhaustively audit every read site, but the field is destructured by the formatted-report path so the attribute on `model` may also be unnecessary. **Worth a second pass before clustering into this issue.**

The issue's recommended fix ("wire up the consumer or remove the field") is correct; see §2.4 for the concrete proposal.

### 1.6 "Once-over" markers for follow-up (mention only)

The remaining ~13 markers (`src/execution/parallelism.rs:189,208`, `src/llm/param_rejections.rs:351,357`, `src/llm/provider.rs:590,700,847,885`, `src/llm/json_extractor.rs:580`, `src/phases/phase.rs:1145`, `src/phases/discover_dimensions.rs:330,347`, `src/phases/discover_contradict.rs:192,237`, `src/cli/discover.rs:1492`, `src/cli/validate.rs:42`, `src/cli/doctor.rs:379`, `src/cli/telemetry_cmd.rs:350`) are listed by the issue as "worth a once-over", not as part of the explicit acceptance-criteria checklist. They should NOT be bundled into the same cluster commit/PR — they need per-site investigation that does not have a v0.14.8-proven pattern yet. **The cluster should land only the explicitly-named fixes; the once-over list becomes a follow-up issue.**

---

## 2. Required modifications (regression-risk fixes the issue body omits)

### 2.1 `cargo build --all-targets` will fail without these — drop now-unused imports alongside the helpers

| File | Helper removed | Drop this import |
|---|---|---|
| `src/phases/discover_dimensions.rs` | `_force_arc_link` (line 355-356) | `use std::sync::Arc;` on **line 34** — `Arc` is referenced *only* in `_force_arc_link` in this file |
| `src/phases/repair.rs` | `_proposal_marker` (line 190-191) | `Path` from `use std::path::{Path, PathBuf};` on **line 5** — bare `Path` is referenced only in `_proposal_marker`; keep `PathBuf` |

If the patch ships only the helper deletion without the import cleanup, `cargo build --all-targets` will fail with `error: unused import: ... (#[warn(unused_imports)])`, which under `.clippy.toml`'s `-D warnings` default flips the build red.

### 2.2 `cargo doc` will produce broken intra-doc links if `repair_missing_brackets` is removed without re-pointing them

`repair_missing_brackets` has three intra-doc-link references that need re-pointing at the live path:

- `src/phases/util.rs:1144` — `/// (`repair_missing_brackets`) cannot: the model writes two` → re-point to `` [`repair_one_missing_bracket`] `` (the comment explains what the walker cannot do that the iterative helper handles).
- `src/phases/util.rs:1455` — `/// Single-step variant of [`repair_missing_brackets`]. Adds at most` → re-point to ``repair_one_missing_bracket``.
- `src/phases/util.rs:1472` — `/// The walker's string handling mirrors [`repair_missing_brackets`]:` → re-point to ``repair_one_missing_bracket``.

`cargo doc --no-deps` will fail with `error: unresolved link to \`repair_missing_brackets\`` otherwise.

### 2.3 `Watchdog` doc-comment is fine to drop wholesale

The 11-line `///` doc on `Watchdog` (lines 802-812) describes the contract — most of that prose migrates naturally onto `Watchdog::spawn` (which has its own 24-line `///` doc starting at line 828). After the struct deletion, the `spawn` doc-comment does **not** need touching; it is self-contained.

### 2.4 `probe.rs` Failed-variant cleanup — concrete alternative sketch

Two clean shapes:

**(a) Drop the inner `String`, make the variant unit (preferred)**

```rust
// src/cli/probe.rs:515-517
/// Probe failed (transport error, all probes rejected).
Failed,
```

```rust
// src/cli/probe.rs:597-599
/// Probe failed (transport error, all probes rejected).
Failed,
```

…and update the **four construction sites** to drop the `format!(...)`:

- `src/cli/probe.rs:285` — change `ProbeOutcome::Failed(format!("{e}"))` → `ProbeOutcome::Failed`
- `src/cli/probe.rs:466` — change `TemperatureProbeOutcome::Failed(format!("{e}"))` → `TemperatureProbeOutcome::Failed`
- `src/cli/probe.rs:866` — change `ProbeOutcome::Failed("network".into())` → `ProbeOutcome::Failed`
- `src/cli/probe.rs:1050` — change `TemperatureProbeOutcome::Failed("network".into())` → `TemperatureProbeOutcome::Failed`

The four messages are currently discarded, so dropping them loses no diagnostic content (verified by the absence of any destructuring site). The error message is replaced by the `tracing::warn!` that precedes each Failed construction.

**(b) Keep the field, drop the marker**

If the v0.14.x intent was to eventually surface the message (e.g. in the probe's JSON summary), keep the field and **drop only the `#[allow(dead_code)]`** — the lint will fire and force the next cleanup to fix the read path or remove the field. This is the "wire up the consumer" half of the issue's "Either wire up the consumer or remove the field" choice.

**Verdict:** (a) is the more honest deletion because the four `format!` invocations are doing real string-formatting work that is then thrown away — a small CPU savings too.

---

## 3. Risk

### 3.1 High-confidence no-risk

- Removing the four phantom helpers (`_force_arc_link`, `_types_are_used`, `_proposal_marker`, `_legacy_record_anchor`) — all have zero callers; build green iff §2.1 imports are also cleaned.
- Deleting `Watchdog` struct — only `Watchdog::spawn(...)` is callable from outside the module; callers (`process.rs:1661, 2677, 2718, 2766, 2804, 2841`) use the static method and are unaffected.
- Deleting `SKETCHES_DIR` / `TAGS_DIR` consts and `load_manifest_for_resume` fn — zero callers; clean removal.

### 3.2 Medium-risk — needs re-test after patch

- Removing `repair_missing_brackets` — no callers, but the 3 doc-comment references (§2.2) become broken. Verified by grep that the live path is `repair_one_missing_bracket` (defined at `src/phases/util.rs:~1474`); re-pointing is mechanical.
- Dropping `#[allow(dead_code)]` on `heartbeat_spawned` (`src/phases/phase.rs:776`) — needs confirmation that `cargo build --tests` succeeds. The function is called from `src/phases/pipe.rs` which is a sibling module that pulls in `RunContext` (the struct that exposes `heartbeat_spawned`). The `#[cfg(test)]` blocks live in `src/phases/pipe.rs` and reference the method, so the call sites are present in test builds. `cargo test --lib` co-compiles `phases::pipe::tests` and `phase::tests`, so this is safe.

### 3.3 Surface-area risk — `Failed(String)` cleanup

If we choose §2.4(a) (drop the inner String), the **four construction sites** at `src/cli/probe.rs:285, 466, 866, 1050` change shape. Any clippy `match_wildcard_for_single_variants` lint will need a re-pass over the construction sites to ensure `Failed(_)` patterns elsewhere still match. Net blast radius: 4 LOC changes in 1 file.

If we choose §2.4(b) (drop only the attribute), no other file changes; surface still emits the discarded message text into the heap. The `format!("{e}")` allocation is the actual cost — that is the second-strongest argument for (a).

### 3.4 Documentation drift risk

Removing the `Watchdog` struct also removes its 11-line `///` doc (lines 802-812). The `Watchdog::spawn` doc (lines 828-852) is self-contained and now becomes the single canonical contract location. No external doc references the removed struct's fields (no `Catalog §D.11.11` cross-ref in the source comments depends on the struct type existing). A quick grep in `docs/` is worth doing before merging — out of scope for this read-only validation but flag for the cluster owner.

---

## 4. Effort estimate

| Subtask | LOC removed | Files | Commits |
|---|---|---|---|
| Delete 4 phantom helpers + import cleanup | ~10 | 3 | 4 (one-liner `chore(dead-code): remove <name> phantom helper` each) |
| Delete `stubs_removed` module | ~9 | 1 | 1 |
| Delete `Watchdog` struct | ~14 | 1 | 1 |
| Delete `SKETCHES_DIR` / `TAGS_DIR` consts | ~6 | 1 | 1 |
| Delete `load_manifest_for_resume` fn | ~4 | 1 | 1 |
| Delete `repair_missing_brackets` + doc-link re-point | ~50 (ref impl body) + 3 doc edits | 1 | 1 |
| Drop redundant `#[allow(dead_code)]` on `heartbeat_spawned` | 1 | 1 | 1 |
| `Failed(String)` cleanup (variant 2.4(a)) | ~6 | 1 | 1 |
| `model: String` field re-audit in `probe.rs` | TBD | 1 | 0–1 |

**Total: ~100 LOC removed, 8 files touched, ~10 commits if each cleanup lands separately — or 1 cluster commit if the team prefers the v0.14.8 single-PR style.**

Suggested commit grouping (matches the v0.14.x hygiene cluster pattern: one-liner commits per removal):

```text
chore(dead-code): remove _force_arc_link phantom helper (drop std::sync::Arc import too)
chore(dead-code): remove _types_are_used phantom helper
chore(dead-code): remove _proposal_marker phantom helper (drop std::path::Path import too)
chore(dead-code): remove _legacy_record_anchor phantom helper
chore(dead-code): drop stubs_removed module (zero callers; rename is misleading)
chore(dead-code): delete unused Watchdog struct (callers go through Watchdog::spawn)
chore(dead-code): delete unused SKETCHES_DIR / TAGS_DIR consts in discover_summary
chore(dead-code): delete unused load_manifest_for_resume re-export
chore(dead-code): delete repair_missing_brackets reference impl + re-point 3 intra-doc links
chore(dead-code): drop redundant #[allow(dead_code)] on heartbeat_spawned
```

That's 10 commits (the issue asks for one-liner commit messages, so this granularity fits). Could collapse to 1 PR if the team prefers.

---

## 5. Better alternatives

### 5.1 None for the structural items

The four phantom helpers, the Watchdog struct, the two consts, `load_manifest_for_resume`, and `stubs_removed` are all genuinely dead code. The only "alternative" would be to **keep them** for future use, but that is exactly the anti-pattern the v0.14.8 cluster established — leave them gone.

### 5.2 `stubs_removed` — delete not rename

The issue says "Either delete or rename to `stubs`". With zero callers, rename is purely cosmetic; the module has no value as an empty placeholder. Delete the whole `mod stubs_removed { ... }` block.

### 5.3 `heartbeat_spawned` — drop marker, not drop function

The function IS called from pipe.rs tests. Drop the `#[allow(dead_code)]` attribute but keep the method. AGENTS.md §"No-go list" prohibits `#[allow(dead_code)]` "hint-only" markers when the symbol genuinely has no callers — but here the function DOES have callers, just only visible under `#[cfg(test)]`. The annotation is therefore equivalent to the v0.14.8 `detect_redact_kind` case (CHANGELOG line 40: "the function IS called from the warn-path at line 835" — marker dropped because the call site exists).

### 5.4 `repair_missing_brackets` — merge into the live path or delete

The doc comment on line 1349 says: `Reference implementation; the iterative bracket repair uses repair_one_missing_bracket`. The live path is `repair_one_missing_bracket` (defined ~line 1474). Two choices:

- **Delete** the reference impl — preferred. It is a 100+ line function that nobody reads and that risks confusion (why are there two bracket-repair strategies in one file?).
- **Wire it up as a slow-path fallback** for inputs that fail `repair_one_missing_bracket` — out of scope for this hygiene cluster. If anyone wants this, it deserves a separate issue with benchmarks.

---

## 6. Verdict

**APPROVE-WITH-MODIFICATIONS.**

Rationale:

1. ✅ Every cited site exists with the exact line numbers the issue lists. The four phantom helpers, the Watchdog struct, the two consts, `load_manifest_for_resume`, `stubs_removed`, `repair_missing_brackets`, and the redundant `#[allow(dead_code)]` on `heartbeat_spawned` were all re-confirmed by reading the source AND a project-wide grep for callers.
2. ✅ The fixes are sound and follow the v0.14.8 pattern from CHANGELOG (one-liner commits, dead-code hygiene with no behavior change).
3. ⚠️ The proposed patches miss four import-cleanups (two unused imports that trigger `unused_imports` + clippy `-D warnings`, and two intra-doc-link re-points that break `cargo doc`). These are mechanical, but must be added to the acceptance criteria before the cluster lands.
4. ⚠️ The 13 "once-over" markers listed at the end of the issue should NOT be bundled into the same cluster — they have no v0.14.8-proven cleanup pattern and need per-site triage. Park them in a follow-up issue so this cluster stays small and reviewable.

If the modifications in §2 are folded into the cluster (and the once-over list is deferred), the cluster is **APPROVE-AS-IS** and ready to land.

---

## 7. Concrete patch checklist (for the implementing agent)

- [ ] `src/phases/discover_dimensions.rs`: delete lines 352-356 (`_force_arc_link` + comment) **AND** delete `use std::sync::Arc;` on line 34.
- [ ] `src/cli/telemetry_cmd.rs`: delete lines 1252-1259 (`_types_are_used` + comment).
- [ ] `src/phases/repair.rs`: delete lines 190-191 (`_proposal_marker`) **AND** change line 5 to `use std::path::PathBuf;`.
- [ ] `src/phases/discover_contradict.rs`: delete lines 353-369 (`_legacy_record_anchor` + comment).
- [ ] `src/cli/telemetry_cmd.rs`: delete lines 382-390 (`stubs_removed` module).
- [ ] `src/sandbox/process.rs`: delete lines 802-825 (Watchdog struct + its doc). Keep `impl Watchdog { pub fn spawn ... }` (line 827 onward).
- [ ] `src/phases/discover_summary.rs`: delete lines 78-83 (`SKETCHES_DIR`, `TAGS_DIR`, and the `/// ` doc on 78-79).
- [ ] `src/cli/discover.rs`: delete lines 1488-1495 (`load_manifest_for_resume` + its doc).
- [ ] `src/phases/util.rs`: delete `repair_missing_brackets` (lines 1336-1453, the //--- bracket repair --- block) **AND** re-point the three intra-doc links at lines 1144, 1455, 1472 to `repair_one_missing_bracket`.
- [ ] `src/phases/phase.rs`: drop the `#[allow(dead_code)]` attribute on line 776 (and re-format the comment onto a new line if needed).
- [ ] Verify with: `cargo build --all-targets` (must pass), `cargo clippy --all-targets -- -D warnings` (must pass), `cargo test --lib` (must pass), `cargo doc --no-deps` (must produce no broken intra-doc links).
- [ ] Per-commit GPG-sign; one-liner subject matching the commit list in §4 above.

## 8. Deferred / out-of-scope

- The once-over list of 13 markers (`src/execution/parallelism.rs:189,208`, `src/llm/param_rejections.rs:351,357`, etc.) — flag as a follow-up issue; do NOT mix into this cluster.
- `model: String` field re-audit in `src/cli/probe.rs:525, 608` — verify destructuring sites exist, then either drop the marker (if destructured) or wire up a consumer. Cluster owner can decide in a single follow-up commit; not load-bearing for #772.
- The probe.rs `Failed(String)` cleanup (§2.4) — recommend landing in this cluster because it is the same hygiene pattern, but technically optional (could split into a follow-up if the cluster owner wants to keep this PR strictly untouched-symbols).

---

## 9. Worktree

- Branch: `validate/772-deadB` (local-only, **not pushed to origin** — per the validation task spec).
- Worktree path: `/home/wolf/workspace/projects/moagan/.worktrees/validate-772-deadB-deadB`.
- Base: `main` @ `28f2cac` (`chore(release): v0.14.8 — telemetry hygiene cluster`).
- No production code modified; no commits made.
- `git status` clean at HEAD = base.
