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

1. `14-integrada-v4.md` (product vision).
2. `T01-06-06b3a1c2.md` (Rust implementation spec, normative).
3. `10-integrada-v0.md` (additive patch catalog, opt-in).

When conflicts arise, T01-06 wins. Catalog patches are opt-in and documented in `docs/ARCHITECTURE.md`.

## Validation gauntlet

Before any commit:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build
```

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
