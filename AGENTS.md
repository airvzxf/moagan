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

The source code (`src/`) is the canonical spec. When documentation and code conflict, **code wins**. ADRs in `docs/adr/` capture the *why* behind decisions; they are historical, not normative.

## Validation tiers

The dev loop splits validation into four tiers so the common cases
(fmt, clippy, build) are sub-30s while the slow integration suite
(~minutes) only blocks push and CI. The compact table:

| Tier | Cost | Where | What |
|---|---|---|---|
| T0 | <2 s | pre-commit | `make fmt-check` + `make guard-deps` |
| T1 | 30–90 s | pre-commit | `make lint` + `make build` |
| T2 | 1–5 min | pre-push | `make test-ci` (cargo test, skips known-flaky `audit_e2e`) |
| T3 | 5–30 min | CI | `make smoke` + `make e2e`; `make e2e-network` on `main` only |

Plus one fast orthogonal check on the commit message itself:

| Hook | Cost | Where | What |
|---|---|---|---|
| commit-msg | <1 s | `commit-msg` hook | `scripts/check-commit-msg.sh` enforces Conventional Commits subject format (`feat:`, `fix:`, `chore(deps):`, …) |

Hooks are managed by [`lefthook`](https://github.com/evilmartians/lefthook);
config lives in [`lefthook.yml`](lefthook.yml). Setup once per clone:

```bash
pacman -S lefthook    # or: cargo install lefthook
lefthook install
```

The local aggregator `scripts/gauntlet.sh` runs everything end-to-end and is
the reference for the full gauntlet order. Branch protection rules and the
required-status-checks list are in [`docs/branch-protection.md`](docs/branch-protection.md).

### Why this split

The user's complaint was the right one: "I don't want to wait 5
minutes for every commit when the project grows." That is solved by
**time-shifting** the slow checks from commit to push, not by
removing them.

- **Commit is frequent** (10–50 / day). Only T0+T1 (<30 s) belongs here.
- **Push is rare** (1–10 / day). T2 is fine on push; the dev is
  about to context-switch anyway while CI runs.
- **CI is the audit**, not the bottleneck. It re-runs everything in a
  clean environment so a corrupted local cache can never mask a real
  regression.
- **Parallelism inside CI** is the second layer of speedup. The 8
  required jobs run concurrently in two rounds; total wall-clock is
  ~6 min cold vs. ~5–8 min sequentially.

### Escape hatches

Use sparingly and document in the commit body when you do:

```bash
# Skip all lefthook hooks for one command
LEFTPHOOK=0 git commit -m "wip: experiment"

# Skip pre-commit + commit-msg only (still runs pre-push on next git push)
git commit --no-verify

# Skip pre-push (still runs CI; CI catches the same checks)
git push --no-verify
```

`--no-verify` does **not** skip CI. Branch protection will still
block the merge if any required check is red. So `git push
--no-verify` is safe-ish: you push faster, the CI catches it, you
fix it.

### Setup on a fresh clone

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

## ⚠️ Red / failed workflows: do NOT merge, repair until green

**The invariant: validation comes before release, never after.**

A tag is irreversible. Once `release.yml` has published a release, a
workflow bug found afterwards can only be fixed with another release.
So the workflow that a change touches must be proven green *while the
change is still revertible* — i.e. on the branch BEFORE any merge, on
the trunk AFTER the merge, and on the release branch BEFORE the tag.
**A red workflow is a stop-the-line event at every step, not a
"fix-it-later" task.** Partial greens are not acceptable; a single
red required check blocks the pipeline until the agent has read the
log, fixed the cause locally, committed + pushed the fix, and
re-dispatched.

```
   ┌─→ push ─→ dispatch ─→ CI red? ─yes─→ read logs ─┐
   │                                                │
   └────── no ──── proceed to next step ─────────────┤
                                                    │
                                  fix locally ←─────┘
                                  commit + sign
                                  push
```

Concretely:

1. Work on a local branch. Implement, commit (GPG-signed), push.
2. Dispatch the affected workflow against the branch:
   `gh workflow run <workflow>.yml --ref <branch>`.
3. Watch it: `gh run watch <run-id>` / `gh run view <run-id> --log-failed`.
4. **If any required job is red**: read the failure log
   (`gh run view <run-id> --log-failed` → narrow to the failed step),
   reproduce locally if possible, fix the cause, commit, push, then
   **go back to step 2**. Do NOT proceed until every required job
   is green. Do NOT dismiss a red job as flake without evidence from
   the log (an empirical reproduction, a stack trace, or a
   deterministic test failure).
5. Only when the branch CI is fully green: open the PR and merge.
6. The trunk now runs CI on the merged commit. **If the trunk CI
   comes back red**: read the trunk run's logs, fix locally on a
   follow-up commit, push, and loop back to step 2. Do NOT open a
   release PR until the trunk is green.
7. Only when the trunk is green: open the release PR (CHANGELOG +
   `Cargo.toml` bump). Run CI on the release branch.
8. **If the release branch CI is red**: same repair loop — read the
   log, fix, commit, push, re-dispatch, repeat. Do NOT tag until the
   release branch is fully green.
9. Only when the release branch is green: merge the release PR.
10. Only after the release PR is merged: tag the **merge commit on
    the trunk**, NOT the branch tip you tagged before opening the PR.
    The squash-merge rewrites the SHA, so the branch-tip tag points
    at a commit that exists on no branch. Re-tag in two steps:
    `git tag -d v0.12.XX && git push origin :refs/tags/v0.12.XX`,
    then `git tag -s v0.12.XX <merge-commit-sha>` and
    `git push origin v0.12.XX`. Verify with
    `git rev-parse v0.12.XX^{commit}` — it MUST match
    `git rev-parse main`. `release.yml` runs on the tag push; an
    orphan tag publishes a release whose SHA no longer matches the
    next merge on main, and the audit log stops being reproducible.
    **Tagging the branch tip and then squashing the PR is the same
    class of bug as tagging before CI is green — both break the
    invariant that the release commit is reachable from main.**

    **Normative procedure — execute these in order, do not skip a
    step.** The `release.yml` workflow has a `verify-tag-reachability`
    job that mechanically enforces invariant ④ on the CI side, plus
    a `verify-tag-signature` job (closes #716) that enforces a new
    invariant: that the tag was signed by a key in
    `.github/trusted-signers` (and, for PGP-signed tags, after
    `.github/trusted-signers.asc` is imported). Both jobs run in
    parallel and `build-release` is gated on both. Defense in depth
    — this procedural list is still load-bearing.

    ① **Fetch and align local `main` to remote.** Covers the local
       drift that the squash merge creates.

       ```bash
       git fetch origin main
       git checkout main
       git reset --hard origin/main
       ```

    ② **Confirm the release bump is at HEAD and `Cargo.toml` matches
       the planned tag.**

       ```bash
       git log --oneline -1   # expect: <sha> chore(release): v0.12.XX — ...
       grep '^version' Cargo.toml   # expect: version = "0.12.XX"
       ```

    ③ **Tag with `-s` (GPG-signed), pointing at the merge SHA.**

       ```bash
       git tag -s v0.12.XX "$(git rev-parse HEAD)"
       git push origin v0.12.XX
       ```

       **Never** tag a local-only commit before it reaches `main`,
       and **never** tag a branch tip that will be squashed.

    ④ **Verify the tag's commit equals `origin/main`'s HEAD.** This
       is the invariant the workflow guard checks.

       ```bash
       [ "$(git rev-parse v0.12.XX^{commit})" \
           = "$(git rev-parse origin/main)" ] \
           || { echo "ORPHAN TAG — abort, re-tag after step ①"; exit 1; }
       ```

    ⑤ **Verify the tag object SHA on the remote matches the local
       one** (catches a partial push / network race).

       ```bash
       [ "$(git rev-parse v0.12.XX)" \
           = "$(git ls-remote origin refs/tags/v0.12.XX | awk '{print $1}')" ] \
           || { echo "TAG PUSH MISMATCH — abort"; exit 1; }
       ```

    ⑥ **Watch the `release.yml` run.** The
       `Verify · tag is reachable from main` job passes if ④ holds;
       `Verify · tag is signed by a trusted signer` passes if `git
       verify-tag vX.Y.Z` succeeds against the allow-list fetched
       from `origin/main` (SSH backend via
       `.github/trusted-signers`, PGP backend via
       `.github/trusted-signers.asc` — used by v0.13.3 and earlier
       tags). Both jobs run in parallel and
       `Build · release binary` cold-builds (~5 min) only after both
       pass; the build is pinned to the immutable tag commit SHA so
       a tag force-push mid-run cannot redirect it. `Publish ·
       GitHub Release` uploads the binary + sha256 + sha512 +
       CycloneDX SBOM and creates the release page.

    **If you discover you orphaned a tag** (e.g. you tagged before
    the squash, or ④ fails):

    ```bash
    git tag -d v0.12.XX                          # delete local
    git push origin :refs/tags/v0.12.XX          # delete remote
    git fetch origin main                        # re-sync to trunk
    git checkout main && git reset --hard origin/main
    git tag -s v0.12.XX "$(git rev-parse origin/main)"
    git push origin v0.12.XX
    ```

    The orphan release page stays published even after the remote
    tag is deleted — GitHub Releases are independent of git refs.
    Mark it as pre-release or delete the release page manually after
    re-publishing at the correct SHA so consumers do not pull the
    orphan binary by accident.

### When the change does not touch CI

Steps 2–4 and 6 collapse into the normal required checks on the PR.
The "loop until green" rule is unchanged — a red required check on
the PR or on the trunk still blocks the merge / tag / release.

### The failure mode this prevents

Never: merge fix → release PR → tag → `release.yml` → *then* discover
the workflow was broken. At that point the release is already public
and the only remedy is another release.

Always: branch green → trunk green → release-branch green → merge →
tag. Any step that comes back red sends the agent back to the repair
loop, never forward.

### Tag signature guard (closes #716)

In addition to the reachability guard (invariant ④), `release.yml`
runs a `verify-tag-signature` job that mechanically checks the tag
was signed by a key on the in-repo allow-list. The allow-list is
**fetched from `origin/main`** (NOT the tagged tree) so revoking a
key on main takes effect on the next release — reading it from the
tagged tree would make revocation structurally impossible because
the trust anchor would be the same object being verified.

- **`.github/trusted-signers`** — SSH backend. Each line is
  `<principal> <key-type> <key-body>`. Used by `git config
  gpg.ssh.allowedsignersfile` so `git verify-tag` can check
  SSH-signed tags. **Matching is by key body, not by principal** —
  git resolves the principal via `ssh-keygen -Y find-principals`
  and accepts the signature if any principal in this file owns the
  key that produced it. Treat every line as an unconditional grant.
  Current entry: `israel.alberto.rv@gmail.com` (ED25519, fingerprint
  `SHA256:POu2Sr8ILb1IM05Vh1cGU3xivjx05QjWoWYhdLc6YHA`).
- **`.github/trusted-signers.asc`** — PGP backend. The maintainer's
  RSA primary key (long ID `414687A3CD7E65B9`, full fingerprint
  `82DE44111B30F91F55BCEB1F414687A3CD7E65B9`) in ASCII-armored
  form. Imported into the runner's keyring **only when the tag
  being verified is PGP-signed** (v0.13.3 and earlier). For
  v0.13.4+ SSH-signed tags the file is optional — this keeps the
  documented key-removal path below intact when the maintainer
  eventually drops the legacy GPG key.

`git verify-tag` auto-detects which backend the tag used, so both
formats are supported without conditional logic in the workflow.
See [`docs/adr/0005-verify-tag-signature-guard.md`](docs/adr/0005-verify-tag-signature-guard.md)
for the design rationale.

**Adding a new trusted signer** requires a PR that:

1. Appends one entry to `.github/trusted-signers` (and, **if the
   new signer uses PGP**, appends a `-----BEGIN PGP PUBLIC KEY
   BLOCK-----` to `.github/trusted-signers.asc`; if all signers
   are SSH-only the `.asc` file may be deleted).
2. Documents the signer's key fingerprint and identity in
   `.github/CONTRIBUTING.md` so the audit log captures who holds
   the signing key.
3. Is reviewed by a co-maintainer when one exists. Today the
   ruleset `protect-main` has `require_code_owner_review: true`
   but `required_approving_review_count: 0`, so the CODEOWNER
   review is auto-requested but not blocking — the maintainer
   merges their own PR. Treat this procedural step as the
   load-bearing control while the repo remains single-maintainer;
   re-evaluate when a co-maintainer joins (see ADR 0005 §"Re-
   evaluation §1" and `docs/branch-protection.md` for the current
   ruleset state).

**Removing a signer** must wait until the most recent tag signed
by that key is at least one minor version old, so a compromise of
the removed key cannot rewrite a release that's in production.
The gauntlet's tag-signature gate (see §"Validation tiers" /
`scripts/gauntlet.sh`) re-verifies every `vX.Y.Z` tag locally on
every run, so a removed signer surfaces immediately for the
operator who removes it.

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
