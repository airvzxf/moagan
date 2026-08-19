# Validation tiers — moagan

This document explains **what** validation runs **where** in the dev loop and **why**
each check lives at the moment it does. Read it once, then trust it.

## The four tiers

| Tier | Cost | Where | What | Why this tier |
|---|---|---|---|---|
| **T0** | <2 s | pre-commit (parallel) | `make fmt-check`, `make guard-deps` | Cheap checks that catch 80 % of "obviously wrong" commits. Fail = don't waste anyone's time. |
| **T1** | 30–90 s | pre-commit (parallel) | `make lint` (`cargo clippy -D warnings`), `make build` | Real lint + the binary actually compiles. Run in parallel since they share no state. |
| **T2** | 1–5 min | pre-push | `make test-ci` (`cargo test --all-targets`, skips known-flaky `audit_e2e` + `cli::diff::*`) | The 21 `tests/integration_*.rs` files. The slow ones. They run before push so the dev catches breakage locally instead of waiting on CI, but they do **not** block commit. |
| **T3** | 5–30 min | CI on PR + post-merge | `make smoke` + `make e2e` (PR); `make e2e-network` (post-merge, fast+explore rows) | Full gauntlet: static smokes, local e2e against the mock pipeline, and the real-LLM e2e (only on `main`, see below). Heavy card80 + 14-model opencode_go sweep + per-provider `--ignored` discovery live in dedicated manual-only workflows. |

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
   │  GitHub Actions — ci.yml (9 parallel jobs)                         │
   │  ───────────────────                                              │
   │                                                                   │
   │  round 1 (no deps, max wall-clock):                               │
   │    ┌─────────────┐  ┌──────────────┐  ┌────────────┐  ┌────────┐ │
   │    │ fmt-check   │  │ guard-deps   │  │ clippy     │  │ build  │ │
   │    │ (T0) ~1s    │  │ (T0) ~1s     │  │ (T1) ~60s  │  │(T1)~60s│ │
   │    └─────────────┘  └──────────────┘  └────────────┘  └────────┘ │
   │                                                                   │
   │  round 2 (depend on build, no artifact sharing):                 │
   │    ┌─────────────┐  ┌──────────────┐  ┌────────────┐  ┌────────┐ │
   │    │ test-lib    │  │ test-tests   │  │ test-doc   │  │ smoke  │ │
   │    │ (T2) ~30s   │  │ (T2) ~3min   │  │ (T2) ~30s  │  │(T3)~2s │ │
   │    └─────────────┘  └──────────────┘  └────────────┘  └────────┘ │
   │    ┌─────────────┐                                               │
   │    │ e2e         │  ← all 5 jobs run `cargo build` themselves;   │
   │    │ (T3) ~1min  │    Swatinem/rust-cache keeps the link step    │
   │    └─────────────┘    at ~5–15 s.                              │
   │                                                                   │
   │  Total wall-clock: ~4 min cold / ~2 min warm                      │
   │  (vs. ~5-8 min before the parallel refactor)                     │
   │                                                                   │
   │  Informational scans (NOT merge gates, run on every PR):         │
   │    • codeql         — Rust security queries (SARIF upload)        │
   │    • cargo-audit   — RustSec advisories JSON artifact             │
   │                                                                   │
   └─────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼ PR merge to main
                                   │
   ┌─────────────────────────────────────────────────────────────────────┐
   │  GitHub Actions — e2e-network.yml (post-merge, auto on main)     │
   │  ────────────────────────────────────────────                      │
   │                                                                   │
   │  e2e-network (2 jobs, real LLM, ~8 min wall-clock)                │
   │    - fast     ~2  min   (timeout-minutes: 55)                     │
   │    - explore  ~8  min   (timeout-minutes: 120)                    │
   │    - both run in parallel after `build-e2e-network`               │
   │      completes; gated by `preflight-minimax`                      │
   │    - builds release binary                                        │
   │    - runs scripts/e2e_audit_proxy.sh with MOAGAN_SMOKE_SECTION=    │
   │      fast|explore                                                 │
   │    - not a PR gate (real-LLM cost)                                │
   │                                                                   │
   │  Manual-only siblings (workflow_dispatch, no auto on main):       │
   │    - e2e-network-card80.yml              ~25 min, MiniMax card80   │
   │    - test-ignored-deepseek.yml           stub (budget exhausted)  │
   │    - test-ignored-opencode-go.yml        stub (budget exhausted)  │
   │    - test-ignored-minimax.yml            ~? min, real --ignored run│
   │                                                                   │
   └─────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼ git tag vX.Y.Z
                                   │
   ┌─────────────────────────────────────────────────────────────────────┐
   │  GitHub Actions — release.yml (tag-triggered)                     │
   │  ────────────────────────────────────                             │
   │                                                                   │
   │  build      → cargo build --release --locked                      │
   │             → SHA-256 / SHA-512 checksums                         │
   │             → CycloneDX SBOM (anchore/sbom-action)                │
   │             → bundle → upload-artifact                            │
   │  publish    → download-artifact                                    │
   │             → softprops/action-gh-release (auto notes, optional  │
   │               prerelease when the tag contains '-')              │
   │                                                                   │
   └─────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │  GitHub ruleset `protect-main` (id 19743104)                      │
   │  ──────────────────────────────────────────                       │
   │                                                                   │
   │  Required checks before merge (all 9 must be green):             │
   │    ✓ fmt-check                                                    │
   │    ✓ guard-deps                                                   │
   │    ✓ clippy                                                       │
   │    ✓ build                                                        │
   │    ✓ test-lib                                                     │
   │    ✓ test-tests                                                   │
   │    ✓ test-doc                                                     │
   │    ✓ smoke                                                        │
   │    ✓ e2e                                                          │
   │                                                                   │
   │  Plus existing rules: deletion, non_fast_forward,                 │
   │  pull_request, required_linear_history. See                      │
   │  docs/branch-protection.md for the gh api block.                 │
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
- **Parallelism inside CI** is the second layer of speedup. The 9 jobs run
  concurrently in two rounds; total wall-clock is ~4 min cold vs. ~5-8 min
  sequentially.

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
merge if any required check is red. So `git push --no-verify` is safe-ish:
you push faster, the CI catches it, you fix it.

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
| Composite action (checkout + toolchain + cache) | [`.github/actions/rust-setup/action.yml`](../.github/actions/rust-setup/action.yml) |
| CI workflow (9 parallel jobs) | [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) |
| CI real-LLM e2e (main only, fast+explore auto) | [`.github/workflows/e2e-network.yml`](../.github/workflows/e2e-network.yml) |
| CI real-LLM e2e (manual-only card80) | [`.github/workflows/e2e-network-card80.yml`](../.github/workflows/e2e-network-card80.yml) |
| CI `--ignored` test runs (post-merge) | [`.github/workflows/test-ignored-{minimax,deepseek,opencode-go}.yml`](../.github/workflows/) |
| CI informational: code scanning | [`.github/workflows/codeql.yml`](../.github/workflows/codeql.yml) |
| CI informational: dependency audit | [`.github/workflows/cargo-audit.yml`](../.github/workflows/cargo-audit.yml) |
| CI release pipeline (tag-triggered) | [`.github/workflows/release.yml`](../.github/workflows/release.yml) |
| Dependabot config (cargo + github-actions) | [`.github/dependabot.yml`](../.github/dependabot.yml) |
| Code ownership / review fan-out | [`.github/CODEOWNERS`](../.github/CODEOWNERS) |
| Security policy (private disclosure channel) | [`.github/SECURITY.md`](../.github/SECURITY.md) |
| Contributing guide | [`.github/CONTRIBUTING.md`](../.github/CONTRIBUTING.md) |
| PR template | [`.github/PULL_REQUEST_TEMPLATE.md`](../.github/PULL_REQUEST_TEMPLATE.md) |
| Issue templates (bug / feature / security) | [`.github/ISSUE_TEMPLATE/`](../.github/ISSUE_TEMPLATE/) |
| GitHub Copilot instructions | [`.github/copilot-instructions.md`](../.github/copilot-instructions.md) |
| Branch protection (ruleset apply) | [`docs/branch-protection.md`](branch-protection.md) |
| Architectural authority | [`docs/proposal-02-rust.md`](proposal-02-rust.md) |