# Copilot instructions — moagan

This file is read by GitHub Copilot (Code Review, Workspace, Chat) when
helping with this repository. It encodes the same constraints as
`AGENTS.md` so the model's first-pass output does not violate the
project's invariants.

## Project one-liner

`moagan` is a multi-agent system for technical problems via massive
solution exploration, curation, and ranking. Single Rust binary. AGPL-3.0.

## Hard rules (never violate)

1. **No** Anthropic SDK crates. If a suggestion includes `anthropic-*`,
   `claude-*`, `claude-sdk`, or anything that talks to Anthropic's
   API directly, **reject it**. The `make guard-deps` check enforces
   this in CI; bypass at your own cost.
2. **No** `secrecy` crate; use `moagan::secret::SecretString` with
   `zeroize`.
3. **No** `unwrap` / `expect` in production code (`src/**/*.rs` minus
   test attribute / examples). Tests may use them.
4. **No** mutable global state, `lazy_static`, `Mutex<Option<T>>`
   for state, or `OnceCell` without a documented reason.
5. **No** `tokio::spawn` without recording the `JoinHandle` or wiring
   a `CancellationToken` parent.
6. **No** secret literals in code, CLI flags, or committed config.
   `RedactWriter` and `RedactPolicy` are the runtime enforcement.
7. **No** `axum`, `hyper`, `sqlx`, `governor`, `figment`, `refinery`,
   `askama`, `handlebars`, `lettre`, `inquire`, `time` crate. Use
   `reqwest` + `rustls` for HTTP, `rusqlite` + `r2d2` for storage,
   `clap` for CLI, `tokio` for async, `serde` + `serde_json` /
   `serde_yaml` / `toml` for serialization.

## Style

- English for code, comments, and documentation. The user-facing CLI
  and TUI copy should match `AGENTS.md`; defer to the user when in
  doubt.
- `Result<T, E>` everywhere. `thiserror` for libraries, `anyhow`
  for `main.rs`.
- Type-driven design: prefer iterators, `?`, `From`/`TryFrom`,
  newtypes, and enums over strings.
- Rust 2024 edition, Rust 1.97.1 stable.

## Commit messages

Conventional commits. Allowed types: `feat`, `fix`, `refactor`,
`docs`, `test`, `chore`, `ci`, `build`, `perf`. The commit-msg hook
(`scripts/check-commit-msg.sh`) enforces this. **Do not** suggest
rephrasing a commit message to break this rule.

## PR flow

The host-level protocol in `~/.config/opencode/AGENTS.md` (the
"GitHub pull request workflow" section) is normative. Copilot should
help draft PR bodies that match
`.github/PULL_REQUEST_TEMPLATE.md`, fill in the validation checklist,
and surface the ruleset `protect-main` (`docs/branch-protection.md`)
context.

## Validation

The four-tier model lives in `AGENTS.md` §"Validation tiers". When
suggesting a change, identify the tier it affects:

- T0 <2 s — `make fmt-check`, `make guard-deps`
- T1 30–90 s — `make lint`, `make build`
- T2 1–5 min — `make test-ci`
- T3 5–30 min — `make smoke`, `make e2e` (CI); `make e2e-network`
  (main only)

If a suggestion increases the T2 wall-clock significantly, propose a
separate `tests/integration_*.rs` file with the slow test instead of
inflating the existing ones.

## When uncertain

Prefer the conservative answer. The `verify` step matters more than
the speed of the answer. If a question requires architectural
authority, refer to the source code (`src/`) first; ADRs in `docs/adr/`
capture the rationale behind major decisions.
