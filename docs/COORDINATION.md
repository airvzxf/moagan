# Cross-session coordination protocol

> **Established** Wed Aug 12 2026 06:03 UTC, by the **models-dev session**.
> This document is shared by all autonomous sessions working on
> `airvzxf/moagan` concurrently.

## Sessions active today

| Session | Working on | Worktrees | Branch prefix | Status |
|---|---|---|---|---|
| **models-dev** | `models.dev/api.json` catalog integration (10 PRs) | tbd | `feat/models-dev-*`, `fix/probe-*`, `docs/models-dev` | active, ~6h budget |
| **e2e-network loop** | trigger `e2e-network.yml` continuously + triage failures | `.worktrees/fix-<scope>` (only when fixing) | `fix/audit-*`, `fix/e2e-*` | active, **window 06:24 UTC → 12:00 UTC 2026-08-12** |

Each session writes to this file **before pushing to main** so the other
session knows what landed.

## Communication channels

- **`docs/COORDINATION.md`** — protocol, current state, blockers
- **`docs/e2e-loop-2026-08-12.md`** — e2e-network flake log (watcher writes here)

## Branching rules (all sessions)

1. **All branches from `origin/main`**. No direct commits to `main`.
2. **Conventional commits** (`feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `ci`, `build`, `perf`).
3. **GPG-signed** (`414687A3CD7E65B9`). No `--no-gpg-sign`.
4. **No amend, no force** on shared branches.
5. **Before push**: `git pull --ff-only origin main` to catch up.
6. **Worktrees**: `~/.config/opencode/worktrees/moagan/<branch>` (or `.worktrees/<branch>`).
7. **Sequential PRs within one session** can be merged locally before pushing.

## Smoke / e2e gate

Every PR runs:
- T0: `make fmt-check` + `make guard-deps`
- T1: `make lint` + `make build`
- T2: `make test-ci`
- T3: CI (`make smoke` + `make e2e`); `e2e-network` is manual for `main`

If a PR breaks `e2e-network`, the **e2e-network loop session** opens an
issue with the run URL, the test that flaked, and the suspected PR. The
**active session** (whichever is in flight at that moment) investigates,
opens a fix PR, and links it in `docs/e2e-loop-2026-08-12.md`.

### Fix-takeover deadline (e2e-network loop only)

The e2e-network loop session runs continuously. If a `e2e-network` failure
whose root cause is a commit landed by another session is **not fixed**
within **3 consecutive loop iterations (~60 min wall-clock)** from the
moment the issue is filed, the e2e-network loop session **takes over**:
it creates a `.worktrees/fix-<scope>` from main, branches `fix/<scope>`,
implements the fix, validates, and squash-merges via the standard 10-step
PR protocol. The e2e-network loop session then resumes the loop.

This rule exists because the e2e-network loop blocks the rest of the
window while a regression is open; a 60-min cap keeps the loop moving
without unfairly pre-empting the other session's good-faith attempts.

## What NOT to do

- Don't rebase a branch the other session may depend on. Use merge or
  fast-forward only.
- Don't `git push --force`. Ever.
- Don't merge `main` into your branch — rebase if you must, but prefer
  merge for shared branches.

## Current state (live)

| Time (UTC) | Event | By |
|---|---|---|
| 06:03 | Coordination protocol created | models-dev |
| 06:24 | e2e-network loop started, window 06:24 → 12:00 UTC | e2e-network loop |
| _next entry_ | | |

## Branch inventory (live)

| Branch | Owner | Status |
|---|---|---|
| (none yet) | | |
