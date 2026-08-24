# Contributing to moagan

Thanks for wanting to make this project better. The full PR protocol
lives in the host-level `AGENTS.md` (the one at
`~/.config/opencode/AGENTS.md`) and the repo-level `AGENTS.md`. This
file is the entry point for both humans and AI agents — it links to
the canonical sources and explains the basics.

## TL;DR

1. **Read `AGENTS.md`** at the repo root. It has the rules. The five
   things it asks you to never do are listed in the *No-go list*
   section. Honoring them keeps the project honest.
2. **Open an issue first** for non-trivial changes. Use the matching
   template (`bug`, `feature`, or `security`).
3. **Branch off `main`** with a conventional-commits prefix:
   `feat(scope):`, `fix(scope):`, `docs:`, `test:`, `ci:`, `chore:`,
   `refactor:`, `perf:`, `build:`.
4. **Run the gauntlet locally** before pushing:
   ```bash
   scripts/gauntlet.sh           # the full T0+T1+T2+T3 pipeline
   ```
   The first line of `scripts/gauntlet.sh` is the source of truth for
   what the CI checks.
5. **GPG-sign every commit** (`git config --local commit.gpgsign true`).
   The global gitconfig already does this; just don't override it.
6. **Open the PR** with the `Closes #N` reference in the body so the
   merge auto-closes the issue.

## Project layout

This is a single-crate Rust binary. The flat layout is intentional —
no workspace, no `crates/` directory. Source under `src/`, tests under
`tests/`, scripts under `scripts/`. Full architectural authority:

| Document | Purpose |
|---|---|
| `docs/proposal-01-concept.md` | Product vision (V4). |
| `docs/proposal-02-rust.md` | Rust implementation spec (T01-06 — normative). |
| `docs/proposal-03-add-ons.md` | Add-on catalog (opt-in overlays). |
| `docs/validation-tiers.md` | The T0–T3 + commit-msg model. |
| `docs/branch-protection.md` | The ruleset `protect-main` and how to update it. |
| `AGENTS.md` | Repo-level rules. Conflicts with the global `AGENTS.md` are resolved by the most specific. |

When you touch anything that changes behaviour, the relevant proposal
gets a sentence in the PR body that links to the section you are
moving.

## Validation tiers — what the local loop looks like

| Tier | Cost | Where | What |
|---|---|---|---|
| **T0** | <2 s | pre-commit | `make fmt-check` + `make guard-deps` |
| **T1** | 30–90 s | pre-commit | `make lint` + `make build` |
| **T2** | 1–5 min | pre-push | `make test-ci` (skips known-flaky `audit_e2e`) |
| **T3** | 5–30 min | CI | `make smoke` + `make e2e`; `make e2e-network` on `main` only |

`make validate` is the local alias for fmt-check + guard-deps + lint +
test + build + smoke. The full gauntlet is `scripts/gauntlet.sh`.

## Conventional commits

The commit-msg hook (`scripts/check-commit-msg.sh`) blocks commits that
don't match:

```
<type>(<scope>): <subject>
```

Allowed types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`,
`ci`, `build`, `perf`. Scope is optional but encouraged for anything
that touches a single module.

## Smoke gates (must pass before handing back)

1. `moagan run --mode fast --provider mock:mock-model` produces
   `final/portfolio.md` and `rankings/ranking.json`.
2. `moagan run --mode fast --provider minimax:MiniMax-M3` with a valid
   `MINIMAX_API_KEY` produces the same artifacts and writes to
   `telemetry/calls.jsonl.gz`.

The first gate is what CI runs. The second gate is owned by the
`e2e-network` workflow on `main` and is the closest thing to a
release signal.

## Security

See `.github/SECURITY.md`. Don't open a public issue for a vuln;
follow the disclosure email workflow.

## Releases

The release pipeline is in `.github/workflows/release.yml`. To cut a
release:

```bash
git tag -s vX.Y.Z -m "release: vX.Y.Z"   # GPG-signed tag
git push origin main vX.Y.Z             # push the branch + the tag
```

The workflow handles the binary build, SHA-256/512 checksums, CycloneDX
SBOM, and the GitHub Release page. The maintainer (`@airvzxf`) is the
only signer today; the key is `414687A3CD7E65B9`.

## License

`moagan` is AGPL-3.0-or-later. By submitting a PR you agree to license
your contribution under the same terms. If that doesn't work for you,
please open the issue first — we can sometimes work something out.
