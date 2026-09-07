# Validation report — issue #788 (CHANGELOG guard)

> Issue: #788 — `ci: guard that a release bump renames the CHANGELOG [Unreleased] heading (v0.14.9 shipped without a [0.14.9] section)`
> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Labels: `ci`, `tech-debt`, `priority:P3`, `size:S`
> Pinned pre-fix SHA: `aeb9e3c72b39e54625f92497208afb3d9418ac43` (v0.14.9 release commit, the one that shipped the bug)
> Pinned fix SHA: `6ce5eac7bf02a4c1043f9abc2369efb73e9eec14` (v0.14.10 cluster)
> Current HEAD on `main`: `ce524e25` (v0.14.10 release)

## TL;DR

The bug is real, reproducible, and worth a guard. **But Assertion 1 has a logical flaw** that lets the exact bug class it claims to catch slip through: the proposed OR clause (`only ## [Unreleased] above the newest release heading`) is true in the pre-fix state and would mark the broken `CHANGELOG.md` green. The fix is one line — drop the OR clause, or replace it with a stricter invariant — but the issue as filed would land a guard that does not guard the case it's named after. Recommendation: **APPROVE-WITH-MODIFICATIONS** — implement the script, but tighten Assertion 1 first, and bundle the footnote backfill into the same PR so CI does not flip red on the rest of `main`.

---

## 1. Confirmed — the historical bug is real

The pre-fix state at `aeb9e3c` ("chore(release): v0.14.9 — Cargo.toml version bump", PR #777) shows exactly the pattern the issue describes:

**Cargo.toml (`git show aeb9e3c:Cargo.toml`):**

```toml
version = "0.14.9"
```

**CHANGELOG.md headings at the same SHA (`git show aeb9e3c:CHANGELOG.md | grep -nE '^## '`):**

```
8:## [Unreleased]                    <- v0.14.9 notes still live here
116:## [0.14.8] - 2026-09-06
164:## [0.14.7] - 2026-09-06
248:## [0.14.6] - 2026-09-06
...
```

There is **no `## [0.14.9] - <date>` heading anywhere in the file** at the moment the release commit ships. The fix commit `6ce5eac` confirms the gap retrospectively — its diff stat is `1 file changed, 31 insertions(+), 37 deletions(-)`, and its commit body explicitly says "rename the stale heading to `[0.14.9] - 2026-09-06` (the tag date) and open a fresh `[Unreleased]`". Verified.

The cluster immediately after (`fab69aa`, closes #778-#781, #791) appended entries (#778 doc fix, #780 dead code, #781 dead-code audit) **into the same `[Unreleased]` block** that already held the shipped v0.14.9 notes — they would have been published as part of v0.14.9's release notes had the issue gone unnoticed.

---

## 2. Current CHANGELOG.md state — version alignment is healthy

**Cargo.toml:** `version = "0.14.10"`

**Top of CHANGELOG.md:**

```markdown
## [Unreleased]
### Fixed — `primary_json_paths` doc-comment claimed a non-existent `index.json` filter (closes #778)
...
## [0.14.10] - 2026-09-07
## [0.14.9] - 2026-09-06
## [0.14.8] - 2026-09-06
...
```

Post-fix, `## [Unreleased]` sits above `## [0.14.10]`, and the order is descending (newest released -> oldest). Version in Cargo.toml matches the latest `## [X.Y.Z]` heading. **Both invariants a clean guard would check hold.**

---

## 3. Missing compare-link footnotes — large but bounded gap

**Defined footnotes (`grep -E '^\[[0-9]+\.[0-9]+\.[0-9]+\]:' CHANGELOG.md`):**

```
[0.14.3]: https://github.com/airvzxf/moagan/compare/v0.14.2...v0.14.3
[0.14.4]: https://github.com/airvzxf/moagan/compare/v0.14.3...v0.14.4
[0.9.2]: https://github.com/airvzxf/moagan/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/airvzxf/moagan/compare/v0.9.0...v0.9.1
```

That's **4 compare-link footnotes**, against **47 `## [X.Y.Z]` headings** in the file (48 total `^## [` headings minus the one `[Unreleased]`). **43 headings are unlinked plain-text versions.**

The issue body claims only `[0.14.3]`, `[0.14.4]`, `[0.9.1]`, `[0.9.2]` are defined — that matches. The issue does NOT enumerate which versions are missing; the backfill is the silent half of the fix. Missing footnote targets:

| Version | Released | Has footnote? |
|---|---|---|
| 0.14.10 | 2026-09-07 | no |
| 0.14.9 | 2026-09-06 | no |
| 0.14.8 | 2026-09-06 | no |
| 0.14.7 | 2026-09-06 | no |
| 0.14.6 | 2026-09-06 | no |
| 0.14.5 | 2026-09-05 | no |
| 0.14.4 | 2026-09-03 | yes |
| 0.14.3 | 2026-09-03 | yes |
| 0.14.2 | 2026-09-03 | no |
| 0.14.1 | 2026-09-03 | no |
| 0.14.0 | 2026-09-02 | no |
| 0.13.6 | 2026-09-02 | no |
| 0.13.5 | 2026-09-02 | no |
| 0.13.4 | 2026-09-02 | no |
| 0.13.3 | 2026-08-31 | no |
| 0.13.2 | 2026-08-30 | no |
| 0.13.1 | 2026-08-30 | no |
| 0.13.0 | 2026-08-29 | no |
| 0.12.18 | 2026-08-29 | no |
| 0.12.17 | 2026-08-28 | no |
| 0.12.16 | 2026-08-28 | no |
| 0.12.15 | 2026-08-28 | no |
| 0.12.14 | 2026-08-28 | no |
| 0.12.13 | 2026-08-28 | no |
| 0.12.12 | 2026-08-28 | no |
| 0.12.11 | 2026-08-28 | no |
| 0.12.10 | 2026-08-27 | no |
| 0.12.9 | 2026-08-27 | no |
| 0.12.8 | 2026-08-27 | no |
| 0.12.7 | 2026-08-27 | no |
| 0.12.6 | 2026-08-27 | no |
| 0.12.5 | 2026-08-27 | no |
| 0.12.4 | 2026-08-27 | no |
| 0.12.3 | 2026-08-27 | no |
| 0.12.1 | 2026-08-27 | no |
| 0.12.0 | 2026-08-26 | no |
| 0.11.2 | 2026-08-26 | no |
| 0.11.1 | 2026-08-26 | no |
| 0.11.0 | 2026-08-26 | no |
| 0.10.0 | 2026-08-24 | no |
| 0.9.12 | 2026-08-23 | no |
| 0.9.11 | 2026-08-23 | no |
| 0.9.6 | 2026-08-21 | no |
| 0.9.5 | 2026-08-21 | no |
| 0.9.4 | 2026-08-21 | no |
| 0.9.2 | 2026-08-20 | yes |
| 0.9.1 | 2026-08-19 | yes |

Note: the very first release (`v0.9.0`) has no `## [0.9.0]` heading in the file at all (it's the implied "start of history" anchor). The first **defined** heading is `## [0.9.1] - 2026-08-19`. The guard's compare-link check should skip the first defined version (it has no prior version to compare against).

---

## 4. Existing guard-script idiom

All six existing guard scripts live under `scripts/check-*.sh`. Conventions:

| File | Extension | Shebang | `set -euo pipefail` | Pattern |
|---|---|---|---|---|
| `check-commit-msg.sh` | `.sh` | `#!/usr/bin/env bash` | yes | regex match + message |
| `check-no-anthropic-sdk.sh` | `.sh` | `#!/usr/bin/env bash` | yes | `grep -nE` + exit codes |
| `check-no-forbidden-crates.sh` | `.sh` | `#!/usr/bin/env bash` | yes | per-line state machine over Cargo.toml + Cargo.lock |
| `check-no-tempdir-leaks.sh` | `.sh` | `#!/usr/bin/env bash` | yes | `grep -rnE` over roots |
| `check-no-trace-debug-in-mod-tests.sh` | `.sh` | `#!/usr/bin/env bash` | yes | `awk` brace tracker |
| `check-non-interactive-env-guard.sh` | `.sh` | `#!/usr/bin/env bash` | yes | `find -print0` + `grep -nE` |

Common shape (representative, from `check-no-anthropic-sdk.sh`):

```bash
#!/usr/bin/env bash
# Fail if any Anthropic SDK crate sneaks into Cargo.toml.
set -euo pipefail

if grep -nE '^(\s*)"?(anthropic[a-z0-9_-]*|claude[a-z0-9_-]*)"\s*=' Cargo.toml; then
    echo "ERROR: forbidden Anthropic SDK crate detected in Cargo.toml" >&2
    exit 1
fi
...
echo "OK: no Anthropic SDK references"
```

Header comment is a long rationale block. Error messages cite the offending file:line and point at the relevant issue / ADR. Final `echo "OK: ..."` on green. Exit code 1 on any violation.

**No `.bash` files anywhere under `scripts/`** (`ls scripts/*.bash` returns ENOENT). The issue body's claim "~60-line bash script (`.bash` conventions + `shellcheck` per the repo's bash standards)" is wrong on two counts:

1. Convention is `.sh`, not `.bash`.
2. No shellcheck gate exists in `Makefile` or `lefthook.yml` (grep for "shellcheck" returns nothing). The repo's bash "standard" is human review + the conventions above; the only mechanical enforcement is the `lefthook` chain itself running the scripts.

---

## 5. Makefile integration — fits cleanly into `guard-deps`

**Current `guard-deps` target** (`Makefile:69-74`):

```make
guard-deps:
	bash scripts/check-no-anthropic-sdk.sh
	bash scripts/check-no-forbidden-crates.sh
	bash scripts/check-no-trace-debug-in-mod-tests.sh
	bash scripts/check-non-interactive-env-guard.sh
```

Four scripts today. Adding the new one is a single-line append:

```make
guard-deps:
	bash scripts/check-no-anthropic-sdk.sh
	bash scripts/check-no-forbidden-crates.sh
	bash scripts/check-no-trace-debug-in-mod-tests.sh
	bash scripts/check-non-interactive-env-guard.sh
	bash scripts/check-changelog-release.sh    # new (T0 tier)
```

`guard-deps` is wired into `lefthook.yml:33-36` (pre-commit, parallel with `fmt-check`, `lint`, `build`) and into `.github/workflows/` (ci.yml round 1). So adding one line puts the new check on every dev commit AND on every CI run — exactly the placement the issue asks for.

`validate` depends on `guard-deps` (`Makefile:60`), so `make validate` (the gauntlet) covers it too.

---

## 6. Guard-design review

### Assertion 1 — **BROKEN AS WRITTEN, must be tightened**

Issue text:

> The version in `Cargo.toml` has a matching `## [X.Y.Z] - YYYY-MM-DD` heading in `CHANGELOG.md`, **or** the only content above the newest release heading is an `## [Unreleased]` section.

Mental execution against the pre-fix state at `aeb9e3c`:

- Cargo.toml = `0.14.9`. No `## [0.14.9]` heading. -> First clause fails.
- Newest release heading = `## [0.14.8]`. Content above it: only `## [Unreleased]`. -> Second clause (the OR) **passes**.
- Overall: **PASS** (incorrectly).

The guard ships green on the exact state it was named after. The OR clause is wrong.

**Root cause of the flaw.** The author seems to be guarding for "mid-cycle" — a state where the dev is preparing the next release, the `[Unreleased]` section has new content, and the next version's heading hasn't appeared yet. But in moagan's actual workflow, `Cargo.toml` is NOT bumped ahead of time. Verified empirically: at `aeb9e3c` (release of v0.14.9), `Cargo.toml` was bumped to `0.14.9` in the same release commit that renamed `[Unreleased]` -> `## [0.14.9]`. Cluster commits (e.g. `fab69aa`) land before the release commit and never touch `Cargo.toml`. So there is no legitimate mid-cycle state where `Cargo.toml = X.Y.Z` but `## [X.Y.Z]` doesn't exist.

**Recommended replacement assertion (one-line tightening):**

> The version in `Cargo.toml` MUST have a matching `## [X.Y.Z] - YYYY-MM-DD` heading in `CHANGELOG.md`.

That's it. No OR clause. The `[Unreleased]` section above the matching heading is the normal "next-cycle work-in-progress" state and is unaffected.

**Mental execution against pre-fix `aeb9e3c`:**

- Cargo.toml = `0.14.9`. No `## [0.14.9]` heading.
- -> **FAIL** with a message like:

  ```
  ERROR: Cargo.toml version '0.14.9' has no matching '## [0.14.9] - <date>' heading in CHANGELOG.md.
  Found latest heading: '## [0.14.8] - 2026-09-06'.
  Did the release commit forget to rename '## [Unreleased]' to '## [0.14.9] - <date>'?
  See scripts/check-changelog-release.sh and issue #788.
  ```

**Mental execution against current `ce524e2`:**

- Cargo.toml = `0.14.10`. `## [0.14.10] - 2026-09-07` exists.
- -> **PASS**.

Correct on both ends.

### Assertion 2 — fine

> At most one `## [Unreleased]` heading.

Sanity check. Current state has exactly one. Pre-fix had exactly one. A future merge collision that added a second `[Unreleased]` block would be caught. No design concerns.

### Assertion 3 — fine, with one caveat

> Every `## [X.Y.Z]` heading has a matching `[X.Y.Z]: https://github.com/...compare/...` footnote.

Two implementation notes:

1. **Allow the first defined version to lack a compare-link.** There is no "previous version" before `## [0.9.1]` (the very first released version is implied to be `v0.9.0` and has no `## [0.9.0]` heading in the file). Keep-a-Changelog expects either an `[X.Y.Z]: https://github.com/.../releases/tag/vX.Y.Z` (release-link) OR an empty compare-link for the first version. The guard should special-case "first defined version" — anything else is a regression that must fail.

2. **Verify the URL points at the right compare range.** The four existing footnotes all match `compare/<prev>...<this>`. A natural extension is to check that `[X.Y.Z]:` cites `v<X-1>...vX.Y.Z` (with `X-1` being the immediately-preceding released version). This is optional — it would catch a copy-paste bug where someone added a footnote with the wrong prev-tag, but it's beyond the issue's scope and the PR can land without it.

Current state fails Assertion 3 (43 missing footnotes). The PR must include the backfill so `make guard-deps` stays green.

---

## 7. Concerns / corrections

| # | Concern | Severity | Recommendation |
|---|---|---|---|
| 1 | Assertion 1's OR clause lets the named bug slip past | **High** | Drop the OR clause (see section 6). One-line change. |
| 2 | Issue says "`.bash` conventions" | Cosmetic | Use `scripts/check-changelog-release.sh` (matches all six existing guards). |
| 3 | Issue says "shellcheck per the repo's bash standards" | Cosmetic | No shellcheck gate exists in `Makefile`/`lefthook.yml`. Drop the claim; rely on the existing human-review convention. |
| 4 | Assertion 3 will fail CI on `main` until the footnote backfill lands | Medium | Bundle the backfill into the same PR (43 entries — mostly mechanical, ~30 min). Do not land the guard first and the backfill in a follow-up — the gap would break `make validate` for every developer. |
| 5 | Assertion 3 doesn't carve out the very first released version | Low | Special-case: the first defined heading (sorted by file order, since the file is descending) is allowed to lack a compare-link. |
| 6 | Assertion 3 doesn't validate the compare-link's `v<prev>` half | Low | Out of scope for this PR. Note in the issue's "Validation" section as a future enhancement. |
| 7 | Script is in `scripts/` but no precedent for a CHANGELOG-aware guard | Cosmetic | The repo's existing guards grep source files; this one greps a docs file. No convention violation — same `scripts/check-*.sh` shape, different input. |

---

## 8. Test plan

Once the script is implemented (with the tightened Assertion 1):

**Synthetic cases (should fail):**

1. **Cargo.toml at a known version, no matching heading.** `sed -i 's/^version = .*/version = "99.99.99"/' Cargo.toml`. Script should fail with "no matching `## [99.99.99]` heading". Revert.
2. **Two `## [Unreleased]` headings.** Duplicate the existing `[Unreleased]` block. Script should fail Assertion 2. Revert.
3. **Remove one compare-link footnote.** Delete the `[0.14.4]` line. Script should fail Assertion 3 (if the implementation treats the first defined heading leniently). Revert.

**Historical case (should fail on pre-fix, pass on fix):**

4. `git stash; git checkout aeb9e3c -- CHANGELOG.md Cargo.toml`. Run `bash scripts/check-changelog-release.sh`. Expect non-zero exit + the message from section 6.
5. `git checkout 6ce5eac -- CHANGELOG.md Cargo.toml`. Run `bash scripts/check-changelog-release.sh`. Expect zero exit + "OK: ..." line.

**Current state (should pass after backfill):**

6. Run on `ce524e2` (current HEAD). Expect zero exit + "OK: ...".

**Release-path simulation:**

7. Apply only the Cargo.toml bump part of `ce524e2` (modify `Cargo.toml` to `0.14.11` but do NOT touch CHANGELOG.md). Run script. Expect non-zero exit with the section 6 message.

---

## 9. Effort estimate

Issue estimates ~2 hours, ~60-line bash script. Breakdown:

| Task | LOC | Effort |
|---|---|---|
| `scripts/check-changelog-release.sh` (ext, shebang, `set -euo pipefail`, header comment with rationale + issue # link) | ~10 | ~10 min |
| Parse `Cargo.toml` `^version = "..."` | ~5 | ~5 min |
| Parse `## [X.Y.Z] - YYYY-MM-DD` headings from CHANGELOG.md | ~10 | ~10 min |
| Assertion 1 (Cargo.toml version has matching heading) — with section 6's tightened logic | ~10 | ~10 min |
| Assertion 2 (<= 1 `[Unreleased]`) | ~5 | ~5 min |
| Assertion 3 (every heading except first has a `[X.Y.Z]:` compare-link footnote) | ~15 | ~15 min |
| Error messages citing issue # + remediation hint | inline | ~10 min |
| `Makefile` `guard-deps` line | 1 | ~1 min |
| CHANGELOG.md footnote backfill (43 entries, sorted by descending version) | 43 in `CHANGELOG.md` | ~30 min (mechanical) |
| Sanity test on `aeb9e3c` (pre-fix) + `6ce5eac` (fix) + `ce524e2` (current) | 3 invocations | ~10 min |
| Total | script ~55 LOC + 43 CHANGELOG lines + 1 Makefile line | ~2 hours |

The 2-hour estimate is realistic. The footnote backfill is the larger surface by line count but mechanical.

---

## 10. Verdict

**APPROVE-WITH-MODIFICATIONS.**

- **Issue is real and worth fixing.** Verified end-to-end.
- **Assertion 1 is broken as written.** The OR clause must be removed (or the whole clause rewritten as the strict invariant in section 6). Without this fix, the guard does not catch the bug class it is named after — the script will report "OK" on the pre-fix `aeb9e3c` state. This is the single most important change.
- **Footnote backfill must ship in the same PR** as the script, otherwise `make guard-deps` flips red on `main` for every developer.
- **Naming / shellcheck nits are cosmetic** (`.sh` not `.bash`; no shellcheck gate to integrate with). Drop those claims from the PR description to match the actual repo conventions.
- **Makefile integration is a one-liner** at `Makefile:69-74`. Wiring through `lefthook.yml` is automatic (the `make guard-deps` step already runs on every pre-commit + every CI round 1).

### Suggested commit message

```
ci(changelog): guard the [Unreleased] rename + backfill 43 compare-link footnotes

The v0.14.9 release (PR #777) shipped without renaming the CHANGELOG
[Unreleased] heading; the shipped v0.14.9 notes stayed under
[Unreleased] and the next cluster's entries were appended into that
block, where they would have been published as part of v0.14.9. PR #793
/ commit 6ce5eac fixed the heading retroactively in the v0.14.10 cluster.

This commit makes the invariant mechanical:

* Add scripts/check-changelog-release.sh, wired into `make guard-deps`
  (T0 tier; runs on every pre-commit and every CI round 1). Three
  assertions:
    1. The version in Cargo.toml has a matching `## [X.Y.Z] - YYYY-MM-DD`
       heading in CHANGELOG.md. (No OR clause — the OR proposed in #788
       was a false-negative that accepted the broken v0.14.9 state.)
    2. At most one `## [Unreleased]` heading.
    3. Every `## [X.Y.Z]` heading except the first has a matching
       `[X.Y.Z]: https://github.com/.../compare/...` footnote.
* Backfill the 43 missing compare-link footnotes for every released
  version between [0.9.2] and [0.14.10] (inclusive), keeping the
  descending-sorted order of the headings.

Closes #788
Refs #777 (the release PR that missed the heading)
```

### Cluster-scope check

This change is CI infrastructure + a docs backfill. It does not touch any Rust source, so:
- `make fmt-check` — N/A (no .rs changes)
- `make lint` — N/A
- `make build` — N/A
- `make test-ci` — N/A
- `make guard-deps` — new gate, must stay green on `main`

The new script itself should be exercised manually on the three test SHAs in section 8 before push, but no automated test harness is needed (the script is its own integration test — `bash scripts/check-changelog-release.sh && echo green || echo red`).
