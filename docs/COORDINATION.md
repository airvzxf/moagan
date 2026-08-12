# Cross-session coordination protocol

> **Established** Wed Aug 12 2026 06:03 UTC, by the **models-dev session**.
> This document is shared by all autonomous sessions working on
> `airvzxf/moagan` concurrently.

## Sessions active today

| Session | Working on | Worktrees | Branch prefix | Status |
|---|---|---|---|---|
| **models-dev** (this) | `models.dev/api.json` catalog integration (10 PRs) | tbd | `feat/models-dev-*`, `fix/probe-*`, `docs/models-dev` | active, ~6h budget |
| **e2e-network watcher** | monitors `e2e-network.yml` for flakes | none (read-only) | none | active, continuous |

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

If a PR breaks `e2e-network`, the **watcher session** opens an issue with
the run URL, the test that flaked, and the suspected PR. The **active
session** (whichever is in flight at that moment) investigates, opens a
fix PR, and links it in `docs/e2e-loop-2026-08-12.md`.

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
| _next entry_ | | |

## Branch inventory (live)

| Branch | Owner | Status |
|---|---|---|
| (none yet) | | |
