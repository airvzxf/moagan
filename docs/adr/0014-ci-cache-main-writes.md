# ADR 0014 — CI cache: only main writes, one job per key, everyone else reads

> **Status**: Accepted
> **Date**: 2026-10-14
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: the cache decisions of #747 (`moagan-test-ignored`
> shared key), #815 (12-hour cleanup), #822 (`moagan-rust-shared-v1`
> for the 8 ci.yml jobs) and the `lookup-only` pull-request setting.
> **Superseded by**: nothing.
> **Relates to**:
> [ADR-0011](0011-no-cancel-in-progress.md) (concurrency of the
> cleanup workflow), AGENTS.md §"Release is cold-by-design".

## Context

Every workflow restores the cargo cache through
`.github/actions/rust-setup` (`Swatinem/rust-cache` v2.9.2). The logs
and the cache listing of 2026-10-14 showed the cache did almost
nothing:

1. **Pull requests built cold and still saved.** `lookup-only: true`
   on `pull_request` means "check the entry exists, do not download
   it" (`src/restore.ts` of the action). `save-if` stayed `true`, so
   each PR saved ~350 MB entries under `refs/pull/N/merge`, which only
   that PR can read.
2. **Main's entry held no build.** The 8 ci.yml jobs shared one key.
   rust-cache saves only when the key has no exact match, so the first
   job to finish won: `fmt-check`, which compiles nothing. The entry
   was 87 MB (the registry) and `make e2e` still compiled 263 crates
   after a "full match".
3. **Collisions and leftovers.** The three `*-doc-sync.yml` jobs are
   all called `sync`, so without a `shared-key` they shared one key.
   `cache-workspace-crates` cached a moagan build the key never
   refreshed. The nightly cleanup deleted any entry idle for 12 hours,
   main's included.

## Decision

- **Only `refs/heads/main` writes.** `rust-setup` saves only when the
  caller passes `save-cache: 'true'` and the ref is main. Pull
  requests, branches and tags restore main's entry (GitHub lets every
  ref read the default branch's caches) and leave nothing behind.
  "Save on the branch and delete at the end of the job" was rejected:
  an entry is only readable by later runs of the same ref, so saving
  it just to delete it is pure cost.
- **Two shared keys, one writer each.**
  - `moagan-dev` (dev/test profile): written by ci.yml's `cache-warm`
    job, which runs on main only, builds the superset the other jobs
    need (`cargo build`, `cargo test --no-run`, `cargo clippy
    --features dev-tools`, the docgen binary) and skips the builds on
    an exact hit. Read by ci.yml, the doc-sync workflows and the
    `--ignored` MiniMax tests.
  - `moagan-release` (`cargo build --release`): written by
    post-release-validation.yml's `e2e-network-on-tag` job (once per
    release, on main). Read by e2e-network.yml and
    e2e-network-card80.yml.
- **Dependencies only.** No `cache-workspace-crates`, no
  `CARGO_INCREMENTAL` override: moagan is rebuilt every run, which it
  would be anyway.
- **`cache-on-failure: false`.** A failed warm build must not freeze a
  partial entry under the exact key.
- **Cleanup keeps main's newest entry per live key and deletes the
  rest**, after every ci / post-release-validation run on main,
  nightly and by hand. The decision is the pure function
  `scripts/ci-cache-prune.sh` (spec: `scripts/test-ci-cache-prune.sh`);
  the live keys are read from the workflows' `shared-key:` lines.
- **Release stays cold** (unchanged): the published binary never
  depends on cache contents.
- **Not cached**: the Rust toolchain. Installing 1.97.1 takes seconds;
  compiling the dependencies takes minutes.

`scripts/check-ci-cache-policy.sh` (in `make guard-deps`) enforces the
rules: save only on main, `cache-on-failure: false`, no
`lookup-only` / `cache-workspace-crates`, no direct use of the cache
actions, a `shared-key` on every rust-setup step, exactly one writer
per key, no `CARGO_INCREMENTAL`, no rust-setup in release.yml.

## Consequences

- The cache holds about two entries (~0.5 GB each) instead of a dozen
  near the 10 GB cap.
- Pull requests get the same warm dependencies as main.
- A Cargo.lock, Cargo.toml (the release bump included) or toolchain
  change makes the first main run after it compile cold while
  `cache-warm` writes the new entry; the cleanup then drops the old
  one.
- A new workflow that needs a cache picks one of the two keys, or adds
  a key together with its single writer; the guard rejects anything
  else.
