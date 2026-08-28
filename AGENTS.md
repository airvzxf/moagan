# Moagan — agent instructions

## Project

`moagan` is a multi-agent system for solving technical problems through massive solution exploration, curation, and ranking. Single Rust binary, multiple modes. Lineage: Rust port of the OpenCode MoA workflow.

## Stack

- Rust stable 1.97.1, edition 2024.
- Single crate (`src/` flat), no workspace.
- One binary `moagan` (`src/main.rs`) + library (`src/lib.rs`).
- Async runtime: `tokio` (full features).
- Storage: SQLite via `rusqlite` + `r2d2` pool, embedded migrations.
- HTTP: `reqwest` + `rustls` (no native OpenSSL).
- LLM: raw HTTP via `reqwest`. **No Anthropic SDK** (CI guard at `scripts/check-no-anthropic-sdk.sh`).
- Privacy: redact-on-write via `RedactWriter` and `RedactPolicy`.
- Errors: `thiserror` for libraries, `anyhow` for `main.rs`.
- Logging: `tracing` + `tracing-subscriber` JSON (every event carries
  `file:line:column` so post-mortem correlation is direct — see
  ADR-0002).
- Runtime coverage: opt-in via the `coverage` Cargo feature and
  `RUSTFLAGS="-Cinstrument-coverage"`; SanCov `*.profraw` files land
  in `<run_dir>/telemetry/coverage/` and are consumed by the
  `moagan coverage show <run_id>` subcommand. See
  [`docs/adr/0002-runtime-coverage.md`](docs/adr/0002-runtime-coverage.md).

## Coding conventions

- All code, comments, and documentation in **English**. The only Spanish interaction is with the user.
- `Result<T, E>` everywhere. No `unwrap`/`expect` in production code; tests may use them.
- Idiomatic Rust: prefer iterators, `?`, `From`/`TryFrom`, newtypes, and enums over strings.
- Type-driven design: every LLM role, status, error class, and mode is an enum or newtype.

## Architectural authority

1. `docs/proposal-01-concept.md` (product vision; spec id V4).
2. `docs/proposal-02-rust.md` (Rust implementation spec, normative; spec id T01-06).
3. `docs/proposal-03-add-ons.md` (additive patch catalog, opt-in; spec id 10-integrada-v0, base T01-06).

When conflicts arise, T01-06 wins. Catalog patches are opt-in and documented inline in
`docs/proposal-03-add-ons.md` (each section is an opt-in overlay).

## Validation tiers

The validation split across the dev loop is documented in
[`docs/validation-tiers.md`](docs/validation-tiers.md). Short version:

| Tier | Cost | Where | What |
|---|---|---|---|
| T0 | <2 s | pre-commit | `make fmt-check` + `make guard-deps` |
| T1 | 30–90 s | pre-commit | `make lint` + `make build` |
| T2 | 1–5 min | pre-push | `make test-ci` (cargo test, skips known-flaky `audit_e2e`) |
| T3 | 5–30 min | CI | `make smoke` + `make e2e`; `make e2e-network` on `main` only |

Hooks are managed by [`lefthook`](https://github.com/evilmartians/lefthook);
config lives in [`lefthook.yml`](lefthook.yml). Setup once per clone:

```bash
pacman -S lefthook    # or: cargo install lefthook
lefthook install
```

The local aggregator `scripts/gauntlet.sh` runs everything end-to-end and is
the reference for the full gauntlet order. Branch protection rules and the
required-status-checks list are in [`docs/branch-protection.md`](docs/branch-protection.md).

## Smoke gates

Two gates must pass before handing to the user:

1. `moagan run --mode fast --provider mock:mock-model` produces `final/portfolio.md` and `rankings/ranking.json`.
2. `moagan run --mode fast --provider minimax:MiniMax-M3` with a valid `MINIMAX_API_KEY` produces the same artifacts and writes to `telemetry/calls.jsonl.gz`.

## Commit policy

- GPG-signed commits are mandatory (`global AGENTS.md`).
- Conventional commits: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `ci`, `build`, `perf`.
- One logical change per commit.
- No `git commit --amend`. No `git push --force`. No `--no-gpg-sign`.

### Release is cold-by-design (no `Swatinem/rust-cache` in `release.yml`)

The release workflow runs on tag pushes (`refs/tags/vX.Y.Z`). Each
tag is its own ref, and `actions/cache` is scoped per-ref: a cache
created for one tag cannot be restored by another tag, even when
the cache key is byte-identical. This is a hard restriction of
GitHub Actions cache — see the "Restrictions for accessing a cache"
section of the dependency-caching docs:

> "Workflow runs also cannot restore caches created for different
> tag names."

The previous `Swatinem/rust-cache` step in `release.yml` therefore
generated a fresh ~270 MB entry per release that was never read
again. Verified empirically on the v0.7.1 run (id `31558391724`):
the action printed `No cache found.` and immediately began
`Downloading crates ...` from crates.io — a full cold build despite
the v0.6.2 entry existing with the same key.

The cold build cost (~5 min per release, 1–2 releases/week) is
acceptable, so the cache step is removed entirely. CI runs on `main`
continue to use `Swatinem/rust-cache` (via `.github/actions/rust-setup`)
because cross-branch restore works there: tag pushes inherit `main`'s
cache scope as a fallback, and same-branch restore hits the cache on
re-runs of the same job.

## Working with workflow (regression-aware)

**The invariant: validation comes before release, never after.**

A tag is irreversible. Once `release.yml` has published a release, a
workflow bug found afterwards can only be fixed with another release.
So the workflow that a change touches must be proven green *while the
change is still revertible* — that is, on the branch and then on
`main`, before any version bump or tag exists.

### When the change touches CI (`.github/workflows/**`, `scripts/*e2e*`, `scripts/*smoke*`)

Local tiers (T0-T2) cannot exercise these paths: `make smoke` and
`make e2e` run against `mock:mock-model`, so only a real dispatch
proves a workflow edit works. Dispatch it from the branch:

1. Work on a local branch. Implement, commit (GPG-signed), push.
2. Dispatch the affected workflow against that branch:
   `gh workflow run <workflow>.yml --ref <branch>`
3. Watch it: `gh run watch <run-id>` / `gh run view <run-id> --log-failed`.
4. Verify **every** required job passes. If any is red, fix the cause,
   commit, push, re-dispatch, and repeat 2-4. Do not proceed on a
   partially green run, and do not dismiss a red job as flake without
   evidence from the log.
5. Open the PR and merge to `main` once required checks are green.
6. Re-validate on `main` if the workflow behaves differently there
   (e.g. steps gated on `github.ref`, or caches scoped per-branch).
7. **Only then** open the release PR (CHANGELOG + `Cargo.toml` bump).
8. After the release PR merges, tag. `release.yml` runs automatically.

### When the change does not touch CI

Steps 2-4 and 6 collapse into the normal required checks on the PR.
The ordering constraint still holds: merge the fix, confirm `main` is
green, and only then bump and tag.

### The failure mode this prevents

Never: merge fix → release PR → tag → `release.yml` → *then* discover
the workflow was broken. At that point the release is already public
and the only remedy is another release.

Always: merge fix → validate the workflow → release PR → tag.

## No-go list

- No Anthropic SDK crates (`anthropic-*`, `claude-*`).
- No `secrecy` crate (use `moagan::secret::SecretString` with `zeroize`).
- No `axum`, `hyper`, `sqlx`, `governor`, `figment`, `refinery`, `askama`, `handlebars`, `lettre`, `inquire`, `time` crate.
- No `comfy-table` crate (deferred to v0.9; see
  [`docs/adr/0001-no-go-list-policy.md`](docs/adr/0001-no-go-list-policy.md)).
- No mutable globals, no `lazy_static`, no `Mutex<Option<T>>` for state.
- No `tokio::spawn` without a `JoinHandle` recorded or a `CancellationToken` parent.
- No secret literals in code, CLI flags, or committed config files.

## Differentiated allow-list (supersedes the blanket prohibition)

Two crates that were historically on the no-go list have explicit
allow-list entries, each guarded by a CI check
([`scripts/check-no-forbidden-crates.sh`](scripts/check-no-forbidden-crates.sh)).
See [`docs/adr/0001-no-go-list-policy.md`](docs/adr/0001-no-go-list-policy.md)
for the full rationale.

- **`petgraph 0.6` + `serde`**: allowed **only** under
  `[dependencies]` with `optional = true` (Cargo feature `dag`).
  Default build (`cargo build` with no features) does **not** pull
  the crate; the linear `phases/` vector stays the default path.
  Bare `petgraph = "0.6"` (non-optional) is rejected.
- **`proptest 1.4`**: allowed **only** under `[dev-dependencies]`.
  A `[dependencies]` row for `proptest` is rejected. The crate does
  not enter the release binary.
