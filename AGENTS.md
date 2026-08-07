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
- Logging: `tracing` + `tracing-subscriber` JSON.

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

1. `moagan run --mode fast --provider mock` produces `final/portfolio.md` and `rankings/ranking.json`.
2. `moagan run --mode fast --provider minimax` with a valid `MINIMAX_API_KEY` produces the same artifacts and writes to `telemetry/calls.jsonl.gz`.

## Commit policy

- GPG-signed commits are mandatory (`global AGENTS.md`).
- Conventional commits: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `ci`, `build`, `perf`.
- One logical change per commit.
- No `git commit --amend`. No `git push --force`. No `--no-gpg-sign`.

## No-go list

- No Anthropic SDK crates (`anthropic-*`, `claude-*`).
- No `secrecy` crate (use `moagan::secret::SecretString` with `zeroize`).
- No `axum`, `hyper`, `sqlx`, `governor`, `figment`, `refinery`, `askama`, `handlebars`, `lettre`, `inquire`, `time` crate.
- No mutable globals, no `lazy_static`, no `Mutex<Option<T>>` for state.
- No `tokio::spawn` without a `JoinHandle` recorded or a `CancellationToken` parent.
- No secret literals in code, CLI flags, or committed config files.
