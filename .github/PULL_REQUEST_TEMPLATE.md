<!--
  Pull request template — moagan

  Fill in the relevant sections. The CI checks below mirror the
  ruleset 'protect-main' required_status_checks list. If a box does
  not apply (e.g. the PR is docs-only and skips test-doc), replace
  it with a one-line justification.
-->

## Summary

> One paragraph. What changed and why. Link the issue this closes with
> `Closes #N` so the squash-merge auto-closes it.

Fixes #

## Type of change

- [ ] `feat` — new user-facing functionality
- [ ] `fix` — bug fix
- [ ] `refactor` — internal change, no behaviour delta
- [ ] `docs` — comment / doc / docs/ change only
- [ ] `test` — only adds or fixes tests
- [ ] `ci` — workflow / Dependabot / lint config
- [ ] `chore` — housekeeping (deps, labels, etc.)
- [ ] `build` — Cargo.toml / Makefile / linker flags
- [ ] `perf` — performance regression fix

## Scope

Touched paths (paste the relevant `git diff --stat` line):

- `src/...`
- `tests/...`
- `docs/...`

If you touched `docs/proposal-*.md`, the architectural authority has
shifted — link the section in the body.

## Validation

Run the matching local gauntlet before pushing. The host-level
`gh-pr-wait.sh` polls CI; this list duplicates the ruleset so a human
reviewer can spot a missing step.

- [ ] **T0** `make fmt-check` and `make guard-deps` both pass
- [ ] **T1** `make lint` (`cargo clippy -D warnings`) and `make build` pass
- [ ] **T2** `make test-ci` passes locally (or the failing test is
  marked `@ignore` with a TODO linked to the tracking issue)
- [ ] **T3** `make smoke` and `make e2e` pass locally
- [ ] **e2e-network** *only if this PR is destined for `main`*: the
  `e2e-network` workflow will run automatically on push; ensure you
  have a local `MINIMAX_API_KEY` if you want to validate it yourself

## Privacy / security checklist

- [ ] No API keys, tokens, or `.env` files committed
- [ ] No Anthropic SDK references (`make guard-deps` enforces this)
- [ ] No forbidden crates added (`make guard-deps` enforces this)
- [ ] No new secrets in logs (`RedactWriter` covers the runtime path;
  if you added a new log statement, make sure it passes through the
  redaction policy)
- [ ] If you discovered a vuln, follow `.github/SECURITY.md` instead
  of opening this PR

## Documentation

- [ ] `// doc` comment added for any new public item
- [ ] `docs/` updated if the public surface or the protocol changes
- [ ] `CHANGELOG.md` (if it exists) updated for user-visible changes

## Deployability

- [ ] No `unsafe` added
- [ ] No `tokio::spawn` without a `JoinHandle` recorded or a
  `CancellationToken` parent
- [ ] No `unwrap` / `expect` in production code (`src/**/*.rs` minus
  `tests/` and examples)
- [ ] No mutable global state

## Local proof

```
$ scripts/gauntlet.sh
… paste the last 10 lines here, including the final OK line …
```

## Cross-references

- Closes #
- Related: #  (optional)
- Blocked by: #  (optional)
