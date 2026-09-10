# ADR 0011 — Never cancel in-progress workflow runs

> **Status**: Accepted
> **Date**: 2026-09-10
> **Deciders**: `airvzxf/moagan` operator (issue #878)
> **Supersedes**: the inline comment in `.github/workflows/ci.yml:10-20`
> (pre-#878) that incorrectly claimed `ci.yml` was *"the ONLY workflow
> in the repo with `cancel-in-progress: true`"*. That claim drifted
> out of truth when [EPIC #852 / PR #874](https://github.com/airvzxf/moagan/pull/874)
> on 2026-09-10 introduced the three doc-sync workflows
> (`cli-doc-sync.yml`, `events-doc-sync.yml`, `test-skips-doc-sync.yml`)
> with the same flag and no update to the comment.
> **Relates to**:
> - [Issue #878](https://github.com/airvzxf/moagan/issues/878) —
>   the filing this ADR formalises (closes #878 when merged).
> - [Issue #738](https://github.com/airvzxf/moagan/issues/738) —
>   closed 2026-09-03 via [PR #739](https://github.com/airvzxf/moagan/pull/739):
>   `test-ignored-*` token-waste incident.
> - [Issue #741](https://github.com/airvzxf/moagan/issues/741) —
>   closed 2026-09-03 via [PR #749](https://github.com/airvzxf/moagan/pull/749):
>   `release.yml` SBOM-upload race on duplicate dispatch.
> - [Issue #742](https://github.com/airvzxf/moagan/issues/742) —
>   closed 2026-09-06 as part of EPIC #762 via
>   [PR #763](https://github.com/airvzxf/moagan/pull/763):
>   defense-in-depth on `codeql.yml` / `cargo-audit.yml`.
> - [EPIC #762](https://github.com/airvzxf/moagan/issues/762) —
>   closed 2026-09-06 via PR #763: the EPIC that closed the
>   pre-#852 concurrency leg. This ADR **extends** the concurrency
>   policy to cover the post-#852 doc-sync surface; it does not
>   re-close #762 (which is already CLOSED).
> - [EPIC #852](https://github.com/airvzxf/moagan/issues/852) /
>   [PR #874](https://github.com/airvzxf/moagan/pull/874) — the
>   2026-09-10 work that added the three doc-sync workflows without
>   updating the now-stale `ci.yml` comment.
> - [`AGENTS.md` §"Validation tiers"](../../AGENTS.md) — the
>   T0/T1/T2/T3 cadence this concurrency rule preserves.
> - [`docs/branch-protection.md`](../branch-protection.md) — the
>   new "Workflow concurrency policy" section this ADR adds.
> - [ADR-0008](./0008-ci-uses-minimax-only.md) — the most recent
>   single-provider CI policy precedent; this ADR is its
>   single-cancel-policy sibling.

## Context

Three prior incidents in 2026-09 established, one workflow at a time,
that cancelling an in-progress run is harmful:

1. **#738** (`test-ignored-minimax.yml`, 2026-09-03): a 14-minute
   cancel of a real-upstream-LLM run wasted the billed token spend AND
   marked the previous commit as a red ❌ on the commit page
   (GitHub renders cancelled runs as failed checks even when the
   workflow is informational — see the *Workflow concurrency policy*
   section in [`docs/branch-protection.md`](../branch-protection.md)).
2. **#741** (`release.yml`, 2026-09-03): two simultaneous `git push`
   dispatches of the same tag raced on the SBOM-upload step
   (`softprops/action-gh-release` tried to dedupe via
   `DELETE /releases/assets/{id}` and the default `GITHUB_TOKEN`
   lacks that scope). The `cancel-in-progress: false` + `queue: max`
   fix serialized the two dispatches FIFO.
3. **#742** (`codeql.yml` + `cargo-audit.yml`, 2026-09-06):
   defense-in-depth on the two scanner workflows so a future PR
   could not silently re-introduce `cancel-in-progress: true`.

The three fixes all turned off `cancel-in-progress` on the affected
workflow. [EPIC #762](https://github.com/airvzxf/moagan/issues/762)
(via [PR #763](https://github.com/airvzxf/moagan/pull/763),
2026-09-06) closed the concurrency leg for the seven pre-#852
workflows. The maintainer's mental model — *"if I push, my last
commit's CI is what I get; nothing is silently killed"* — is the
unifying principle behind all three.

Then on 2026-09-10, [EPIC #852 / PR #874](https://github.com/airvzxf/moagan/pull/874)
introduced three new doc-sync workflows (`cli-doc-sync.yml`,
`events-doc-sync.yml`, `test-skips-doc-sync.yml`) and copy-pasted the
same `cancel-in-progress: true` flag the prior incidents had ruled
out. The misleading `ci.yml:12` comment ("the ONLY workflow in the
repo with `cancel-in-progress: true`") was not updated, hiding the
regression. Empirical confirmation via `gh run list`:

| Run ID | Workflow | Branch | Cancellation pattern |
|---|---|---|---|
| 33675508929 | ci | `refactor/rename-providers-legacy-686` | killed by run 33675536963 (same branch, 17s later) |
| 33684567275 | ci | `release/v0.13.6` | killed by run 33684848208 (same branch, 3 min later) |
| 33152754447 | ci | `fix/card80-discovers-audit-2.3` | killed by newer push on same PR |

The audit-trail cost is concrete: the v0.13.6 run (33684567275) had
two real failures (`T3 · smoke`, `T3 · e2e` at 21:20:37-40) followed
by two cancellations (`T2 · test-tests`, `T2 · test-lib` at
21:23-24). The operator must triage four red ❌ marks on a single
commit to learn that two were real and two were collateral damage.

## Decision

### 1. Two-pattern rule

Workflows in `airvzxf/moagan` use one of two concurrency patterns.
A workflow that falls into neither is non-conformant and must
amend this ADR.

- **Pattern A — `run_id`-keyed, no cancel flag.** Use
  `group: <workflow>-${{ github.run_id }}` + `cancel-in-progress: false`
  (the `cancel-in-progress: false` is explicit for clarity; the
  default is also `false`). Every dispatch becomes its own
  concurrency group; nothing cancels, nothing queues behind an
  in-flight run.

  | Workflow | Why Pattern A |
  |---|---|
  | `ci.yml` | mock LLM provider, no upstream cost; wall-clock bounded by runner quota, not by token spend. PR iteration benefits from each push producing an independent run. |
  | `cli-doc-sync.yml` | docgen ≤ 5 min, mock toolchain, single job. |
  | `events-doc-sync.yml` | docgen ≤ 5 min, mock toolchain, single job. |
  | `test-skips-doc-sync.yml` | docgen ≤ 10 min, mock toolchain, single job. |
  | `e2e-network.yml` | manual dispatch only; no push trigger so burst-queue isn't a concern. Already used `run_id` before this ADR. |
  | `e2e-network-card80.yml` | manual dispatch only. Already used `run_id` before this ADR. |

- **Pattern B — ref-keyed, `queue: max`, no cancel flag.** Use
  `cancel-in-progress: false` + `queue: max` with a group key
  derived from the ref / tag / commit SHA. Backpressure matters
  here because duplicate dispatches have a real cost (upstream
  tokens, Release-asset race, or per-job scanner load).

  | Workflow | Why Pattern B |
  |---|---|
  | `test-ignored-minimax.yml` | real upstream LLM calls (`api.minimax.io/anthropic/v1/models`, token-billed); per #738. Group key splits by `github.event_name` so a manual dispatch never queues behind a slow push run. |
  | `release.yml` | publishes a GitHub Release; per #741 SBOM race. Group key includes the tag so parallel releases of distinct versions are unblocked. |
  | `post-release-validation.yml` | real-LLM audit on the tag commit; per #761. |
  | `cargo-audit.yml` | queries RustSec DB; per #742 defense-in-depth. |
  | `codeql.yml` | GitHub-native scanner; per #742 defense-in-depth. |

### 2. `cancel-in-progress: true` is forbidden

No workflow in the repo may set `cancel-in-progress: true`. New
workflows default to Pattern A unless they fall into Pattern B
(per the table above). A new Pattern B workflow must add itself
to the Compliance table below.

### 3. Inline comments must point at this ADR

Every workflow's `concurrency:` block carries a comment that
cites this ADR by number. Future maintainers who copy-paste a
workflow into this repo see the convention immediately.

### 4. Enforcement

A new CI guard `scripts/check-no-cancel-in-progress.sh` (modeled
on `scripts/check-no-forbidden-crates.sh`, ADR-0001's enforcement
sibling) fails any PR that introduces `cancel-in-progress: true`
in `.github/workflows/*.yml`. The guard is wired into
`lefthook.yml` pre-commit, `Makefile`'s `make guard-deps`, and
`scripts/gauntlet.sh`. The guard is the load-bearing control;
this ADR and the inline comments are supporting
defense-in-depth.

## Consequences

### Positive

- **Audit-trail integrity.** No PR-iteration commit ever renders
  red because a follow-up push killed the previous run. The commit
  page shows the true conclusion of every check.
- **No wasted upstream spend.** No real-LLM call is killed
  mid-flight by a follow-up push.
- **No operator surprise.** A push's CI result is always the
  push's CI result. Cancellation was a silent side effect that
  no per-run UX warned about.
- **Regression is blocked at CI time.** A future PR cannot silently
  re-introduce `cancel-in-progress: true`; the guard fails the
  build before merge.

### Negative

- **Actions-minute cost on iterative PRs.** A 4-push PR burst now
  runs `ci.yml` ~4× in parallel instead of cancelling each prior
  run. On a runner-quota-constrained org this would be expensive;
  on this repo the Actions budget already absorbs the parallel
  load (the 8 jobs run in 2 rounds anyway — see `AGENTS.md`
  §"Validation tiers"). Accepted.
- **Doc-sync cost on iterative PRs.** A PR with N pushes runs N
  docgen builds (≤ 5–10 min each). Cumulative cost is bounded by
  PR review time, not by push frequency. Accepted.

### Compliance

| Workflow file | Pattern | Justification | Where enforced |
|---|---|---|---|
| `ci.yml` | A | mock LLM, ≤ 6 min | this ADR |
| `cli-doc-sync.yml` | A | docgen ≤ 5 min | this ADR |
| `events-doc-sync.yml` | A | docgen ≤ 5 min | this ADR |
| `test-skips-doc-sync.yml` | A | docgen ≤ 10 min | this ADR |
| `e2e-network.yml` | A | manual dispatch only | this ADR (pre-existing) |
| `e2e-network-card80.yml` | A | manual dispatch only | this ADR (pre-existing) |
| `test-ignored-minimax.yml` | B | upstream token spend | this ADR + #738 |
| `release.yml` | B | Release-asset race | this ADR + #741 |
| `post-release-validation.yml` | B (auto) / A (dispatch) | tag-commit audit + manual re-validation | this ADR + #761, #882; group key splits by `github.event_name` so a manual dispatch never queues behind a slow `workflow_run` (mirrors `test-ignored-minimax.yml`'s split) |
| `cargo-audit.yml` | B | RustSec DB queries | this ADR + #742 |
| `codeql.yml` | B | CodeQL scanner | this ADR + #742 |
| `cleanup-actions-cache.yml` | (no concurrency block) | nightly cron + manual dispatch; no PR trigger; irrelevant | n/a |

This table is the single auditable surface that makes the policy
enforceable in code review. A new workflow that adds itself to
the repo must add itself here.

## Alternatives considered

### A. Drop `concurrency:` entirely

Rejected. Loses the FIFO serialization that #741 added to
`release.yml`; the SBOM race returns. Also weakens
`test-ignored-minimax.yml`'s token-spend guard.

### B. `queue: max` everywhere

Rejected. Serializes per-PR across all 11 workflows, so a 4-push
PR iteration takes ~4× the wall-clock instead of ~1×. The
`run_id`-per-push pattern keeps each push's result available in
roughly one run's wall-clock, which is the desired PR-iteration
UX.

### C. Keep `ci.yml` on `cancel-in-progress: true` as an exception

Rejected. Maintains the misleading "ONLY workflow" comment, leaves
the operator-surprise class of bug open on the highest-traffic
workflow, and the Actions-minute cost argument is moot on this
repo (see Negative above).

### D. `group: ${{ github.sha }}`

Rejected. A manual dispatch against `main` reuses the current
HEAD's SHA, so under Pattern B it queues behind any in-flight run
for the same SHA. Under Pattern A's `cancel-in-progress: true`
it would kill the prior run — strictly worse. `run_id` is unique
per dispatch and is the right identifier.

## Re-evaluation

This ADR is revisited when any of the following happen:

1. A new workflow is added that falls into neither Pattern A nor
   Pattern B. The PR must amend this ADR with the new pattern
   and an entry in the Compliance table before the workflow
   can be merged. The guard `scripts/check-no-cancel-in-progress.sh`
   enforces this by failing any future PR that introduces a
   `cancel-in-progress: true` flag (Pattern B is allowed; the
   guard only rejects `cancel-in-progress: true`).
2. The Actions runner quota on this repo becomes binding and the
   Pattern-A parallel cost becomes a real concern. Mitigation:
   move heavy patterns to Pattern B by amendment (mirrors ADR-0001
   §"Re-evaluation").
3. A future PR proposes re-enabling `cancel-in-progress: true`
   on any workflow. That PR must first amend this ADR with a
   Re-evaluation section and re-vote the decision (no silent
   relaxation; mirrors ADR-0001 §"Re-review trigger"). The guard
   script makes this amendment a hard prerequisite: removing
   `cancel-in-progress: true` from the guard script requires
   editing this ADR in the same change.
4. A future PR proposes splitting Pattern A into two sub-patterns
   (e.g. "A1: docs-only" vs "A2: tests"). Evaluate on its merits;
   the Compliance table can absorb the sub-pattern without an ADR
   amendment as long as the rule `cancel-in-progress: true` is
   forbidden remains intact.

## References

- [Issue #738](https://github.com/airvzxf/moagan/issues/738) /
  [PR #739](https://github.com/airvzxf/moagan/pull/739) — the
  original token-waste incident for `test-ignored-*`.
- [Issue #741](https://github.com/airvzxf/moagan/issues/741) /
  [PR #749](https://github.com/airvzxf/moagan/pull/749) — the
  SBOM-upload race for `release.yml`.
- [Issue #742](https://github.com/airvzxf/moagan/issues/742) /
  [PR #763](https://github.com/airvzxf/moagan/pull/763) —
  defense-in-depth for `codeql.yml` / `cargo-audit.yml`.
- [EPIC #762](https://github.com/airvzxf/moagan/issues/762) —
  the EPIC that closed the pre-#852 concurrency leg; this ADR
  extends the policy to the post-#852 surface.
- [EPIC #852](https://github.com/airvzxf/moagan/issues/852) /
  [PR #874](https://github.com/airvzxf/moagan/pull/874) — the
  2026-09-10 doc-sync work that re-introduced the regression
  class this ADR closes.
- [Issue #878](https://github.com/airvzxf/moagan/issues/878) —
  the filing this ADR formalises.
- [ADR-0001](./0001-no-go-list-policy.md) — the differentiated
  no-go-list ADR whose `Re-evaluation` pattern this ADR follows,
  and whose enforcement sibling (`scripts/check-no-forbidden-crates.sh`)
  `scripts/check-no-cancel-in-progress.sh` is modeled on.
- [ADR-0008](./0008-ci-uses-minimax-only.md) — the most recent
  single-provider CI policy precedent; this ADR is its
  single-cancel-policy sibling.
