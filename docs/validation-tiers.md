# Validation tiers — moagan

This document explains **what** validation runs **where** in the dev loop and **why**
each check lives at the moment it does. Read it once, then trust it.

## The four tiers

| Tier | Cost | Where | What | Why this tier |
|---|---|---|---|---|
| **T0** | <2 s | pre-commit (parallel) | `make fmt-check`, `make guard-deps` | Cheap checks that catch 80 % of "obviously wrong" commits. Fail = don't waste anyone's time. |
| **T1** | 30–90 s | pre-commit (parallel) | `make lint` (`cargo clippy -D warnings`), `make build` | Real lint + the binary actually compiles. Run in parallel since they share no state. |
| **T2** | 1–5 min | pre-push | `make test-ci` (`cargo test --all-targets`, skips known-flaky `audit_e2e`) | The 21 `tests/integration_*.rs` files. The slow ones. They run before push so the dev catches breakage locally instead of waiting on CI, but they do **not** block commit. |
| **T3** | 5–30 min | CI on PR + post-merge | `make smoke` + `make e2e` (PR); `make e2e-network` (post-merge) | Full gauntlet: static smokes, local e2e against the mock pipeline, and the long real-LLM e2e (only on `main`, see below). |

Plus one fast orthogonal check on the commit message itself:

| Hook | Cost | Where | What |
|---|---|---|---|
| **commit-msg** | <1 s | `commit-msg` hook | `scripts/check-commit-msg.sh` enforces Conventional Commits subject format (`feat:`, `fix:`, `chore(deps):`, …). |

## The dev loop, end-to-end

```
   ┌─────────────────────────────────────────────────────────────────────┐
   │  local                                                            │
   │  ─────                                                            │
   │                                                                   │
   │  $ edit src/...                                                  │
   │  $ git add .                                                     │
   │  $ git commit -m "feat(llm): add streaming parser"               │
   │     │                                                             │
   │     ├─► commit-msg    ─ check format ─── pass                     │
   │     │                                                             │
   │     └─► pre-commit    ─ T0: fmt-check + guard-deps ── parallel    │
   │                       ─ T1: lint + build ──────── parallel        │
   │                       Total wall-clock: ~30–90 s                  │
   │                                                                   │
   │  $ git push                                                       │
   │     │                                                             │
   │     └─► pre-push      ─ T2: cargo test ─── sequential              │
   │                       Total wall-clock: 1–5 min                  │
   │                                                                   │
   └─────────────────────────────────────────────────────────────────────┘
                                  │
                                  ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │  GitHub                                                            │
   │  ──────                                                           │
   │                                                                   │
   │  PR opened (web or `gh pr create`)                                │
   │     │                                                             │
   │     ├─► ci.yml :: lint-test-build  (T0+T1+T2 on runner)           │
   │     └─► ci.yml :: smoke-e2e        (T3 static + local e2e)       │
   │                                                                   │
   │  merge to main                                                    │
   │     │                                                             │
   │     └─► e2e-network.yml            (~25 min, real LLM)            │
   │                                                                   │
   │  branch protection (post-setup) requires                           │
   │    ✓ lint-test-build                                              │
   │    ✓ smoke-e2e                                                    │
   │  before merge button is enabled.                                  │
   │                                                                   │
   └─────────────────────────────────────────────────────────────────────┘
```

## Why this split

The user's complaint was the right one: "I don't want to wait 5 minutes for every
commit when the project grows." That is solved by **time-shifting** the slow
checks from commit to push, not by removing them.

- **Commit is frequent** (10–50 / day). Only T0+T1 (<30 s) belongs here.
- **Push is rare** (1–10 / day). T2 is fine on push; the dev is about to
  context-switch anyway while CI runs.
- **CI is the audit**, not the bottleneck. It re-runs everything in a clean
  environment so a corrupted local cache can never mask a real regression.

## Escape hatches

Use sparingly and document in the commit body when you do:

```bash
# Skip all lefthook hooks for one command
LEFTPHOOK=0 git commit -m "wip: experiment"

# Skip pre-commit + commit-msg only (still runs pre-push on next git push)
git commit --no-verify

# Skip pre-push (still runs CI; CI catches the same checks)
git push --no-verify
```

`--no-verify` does **not** skip CI. Branch protection will still block the
merge if T3 is red. So `git push --no-verify` is safe-ish: you push faster,
the CI catches it, you fix it.

## Setup on a fresh clone

```bash
# One-time per machine
pacman -S lefthook           # or: cargo install lefthook
lefthook --version           # ≥ 1.6 required

# One-time per repo clone
lefthook install             # writes .git/hooks/{pre-commit,pre-push,commit-msg}
git config --local commit.gpgsign true    # if not already global
```

Verify the hooks are wired:

```bash
$ ls -1 .git/hooks/{pre-commit,pre-push,commit-msg}
.git/hooks/commit-msg
.git/hooks/pre-commit
.git/hooks/pre-push
$ head -1 .git/hooks/pre-commit
#!/usr/bin/env lefthook
```

## What lives where in the repo

| Concern | Location |
|---|---|
| Hook config (committed) | [`lefthook.yml`](../lefthook.yml) |
| Conventional commit check | [`scripts/check-commit-msg.sh`](../scripts/check-commit-msg.sh) |
| Local validator aggregator | [`scripts/gauntlet.sh`](../scripts/gauntlet.sh) (`--fast`, `--skip-smoke`, …) |
| Makefile targets (`validate`, `fmt-check`, `lint`, `test-ci`, `smoke`, `e2e`, `e2e-network`) | [`Makefile`](../Makefile) |
| CI lint+test+build | [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) job `lint-test-build` |
| CI smoke + local e2e | [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) job `smoke-e2e` |
| CI real-LLM e2e (main only) | [`.github/workflows/e2e-network.yml`](../.github/workflows/e2e-network.yml) |
| Branch protection (run once) | [`docs/branch-protection.md`](branch-protection.md) |
| Architectural authority | [`docs/proposal-02-rust.md`](proposal-02-rust.md) |
