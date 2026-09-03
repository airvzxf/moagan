# ADR 0005 — `verify-tag-signature` job: GPG/SSH signature guard for release tags

> **Status**: Accepted
> **Date**: 2026-09-02
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: nothing (adds a new structural check alongside the
> existing `verify-tag-reachability` job).
> **Relates to**:
> [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
> (the `verify-tag-signature` job),
> [`.github/trusted-signers`](../../.github/trusted-signers) (SSH allow-list),
> [`.github/trusted-signers.asc`](../../.github/trusted-signers.asc) (GPG allow-list),
> [`AGENTS.md` §"Tag signature guard"](../../AGENTS.md),
> [`.github/CONTRIBUTING.md` §"Releases"](../../.github/CONTRIBUTING.md),
> [`docs/branch-protection.md`](../branch-protection.md) (ruleset
> `protect-main` carries the `required_signatures` rule for *commits*
> but not for *tags* — this ADR fills that gap).

## Context

The ruleset `protect-main` already enforces **commit signing** on
`main` via the `required_signatures` rule (see
[`docs/branch-protection.md`](../branch-protection.md) §"Current
state"). Every commit landing on `main` must be GPG/SSH-signed, and
the local `commit.gpgsign=true` config (combined with
`lefthook`'s `pre-commit` chain) backs that up.

**Tags are a different surface.** The `release.yml` workflow
currently has one tag guard — `verify-tag-reachability`, which
proves the tag's commit is an ancestor of `origin/main` (invariant
④ from `AGENTS.md`). It does **not** verify the tag's signature.
Two failure modes slip through:

1. **Unsigned tag pushed by a leaked PAT.** If a contributor's
   personal access token is exposed, an attacker could push
   `vX.Y.Z` (unsigned, but pointing at a real commit on `main`)
   and trigger a release build for an arbitrary version.
   `verify-tag-reachability` would pass because the commit is
   reachable; the release would publish.
2. **Tag forged on a fake commit.** The reachability guard would
   catch this (the commit wouldn't be reachable from `main`), but
   only because reachability is also checked — the signature
   check is independent and complements it.

The procedural rule "`git tag -s vX.Y.Z`" in `AGENTS.md` and
`.github/CONTRIBUTING.md` covers *correct* operation. Issue #716
asks for the **structural** counterpart — a CI job that fails the
workflow when the rule is not followed.

Three complicating factors surfaced during exploration of the
actual repository state (2026-09-02):

- **The signing format has changed over the project's lifetime.**
  Tags `v0.5.0` through `v0.13.3` are PGP-signed with the
  maintainer's RSA key (`82DE44111B30F91F55BCEB1F414687A3CD7E65B9`,
  long ID `414687A3CD7E65B9`). Tags `v0.13.4`, `v0.13.5`, `v0.13.6`
  and `v0.14.0` (the current HEAD) are SSH-signed with the
  maintainer's ED25519 key (principal
  `israel.alberto.rv@gmail.com`, fingerprint
  `SHA256:POu2Sr8ILb1IM05Vh1cGU3xivjx05QjWoWYhdLc6YHA`). Both
  formats must continue to
  verify — an old release branch that re-tags an older commit must
  not be forced to re-sign with the current format.
- **The CI runner has no keys by default.** A fresh GitHub Actions
  runner image has no GPG public keys and no SSH allowed-signers
  file. The new job must configure both before running
  `git verify-tag`, or the verification always fails.
- **`git config gpg.format` is ambiguous about verification.** Setting
  `gpg.format ssh` for the SSH signing backend does **not** block
  GPG verification — `git verify-tag` auto-detects which backend
  the tag's signature uses, and consults the matching trust source.
  So both formats can be verified in the same workflow run without
  conditional logic.

## Decision

Add a new `verify-tag-signature` job to `.github/workflows/release.yml`
that runs **in parallel** with `verify-tag-reachability`, gates both
on `build-release`, and mechanically enforces "the tag was signed
by a key on the in-repo allow-list". Two allow-list files support
the dual-format history:

- `.github/trusted-signers` — SSH backend. Format
  `<principal> <key-type> <key-body>`. Consulted via
  `git config gpg.ssh.allowedsignersfile`. **Matching is by key
  body, not by principal** — git resolves the principal via
  `ssh-keygen -Y find-principals` and accepts the signature if any
  principal in the file owns the key that produced it. The
  `<principal>` field is a label that documents which identity the
  key represents, not a constraint; treat every line as an
  unconditional grant.
- `.github/trusted-signers.asc` — PGP backend. ASCII-armored
  public key block. Imported via `gpg --import` into the runner's
  ephemeral keyring. The job consults this file **only when the tag
  being verified is PGP-signed** (v0.13.3 and earlier), so it is
  optional for SSH-only releases — this keeps the documented key-
  removal path in AGENTS.md §"Tag signature guard" intact when the
  maintainer eventually drops the legacy PGP key.

Both files are committed to the repository. Public keys are not
secrets; committing them lets `git verify-tag` work in CI without
any secret-management ceremony, and lets the audit log (`git log
-- .github/trusted-signers*`) capture every signer change.

> **Note on the ruleset**: the ruleset `protect-main` has
> `require_code_owner_review: true` but
> `required_approving_review_count: 0`, so the CODEOWNER review is
> auto-requested but not blocking — the maintainer merges their own
> PR. The procedural rule in AGENTS.md §"Tag signature guard" is
> the load-bearing control while the repo remains single-
> maintainer. Re-evaluate when a co-maintainer joins (see
> §"Re-evaluation §1").

### D-1 — In-repo allow-list, not a GitHub secret

Public keys committed to the repo. The alternative — passing them
via `${{ secrets.* }}` — would:

- Require `actions/checkout` → secret-injection orchestration
  that adds latency to the verification step.
- Hide the allow-list from `git log` / blame, defeating the audit
  log the procedural rule is supposed to create.
- Force every co-maintainer to add their public key to the repo
  settings *and* to the in-repo allow-list, doubling the surface
  area for inconsistency.

The cost of in-repo is small: a public key is public, and
committing it makes the trust boundary auditable.

### D-2 — Two files, one per backend, not one file with a superset

`.github/trusted-signers` (SSH) is a plaintext allow-list in the
format git expects; OpenSSH's `sshsig.c` skips `#`-prefixed lines
anywhere in the file, so the explanatory header is safe to keep.

`.github/trusted-signers.asc` (PGP) is an OpenPGP packet — no
comments, no prefix allowed, the file must start with
`-----BEGIN PGP PUBLIC KEY BLOCK-----`. Mixing the two formats in
one file would break the PGP parser. Two files, each format
strict, no parser ambiguity.

### D-3 — `gpg.format ssh` + GPG keyring, not exclusive

The verify step sets `gpg.format ssh` *and* imports the PGP public
key. This is intentional:

- `gpg.format ssh` controls the **signing** backend git uses when
  creating new signatures; it does **not** restrict verification.
- `git verify-tag` auto-detects whether the tag was signed with
  SSH or PGP and consults the matching trust source.
- A v0.13.3-era re-release (`v0.13.7` cut from a security
  backport, say) verifies against the PGP backend without any
  workflow change.

Empirically verified on the actual local checkout (2026-09-02):

```bash
$ git config gpg.format ssh
$ git config gpg.ssh.allowedsignersfile .github/trusted-signers
$ git verify-tag v0.14.0    # SSH-signed
Good "git" signature for israel.alberto.rv@gmail.com with ED25519 key SHA256:POu2Sr8ILb1IM05Vh1cGU3xivjx05QjWoWYhdLc6YHA
$ git verify-tag v0.13.0    # PGP-signed (v0.13.0 tag was RSA GPG)
gpg: Good signature from "Israel Roldan (airvzxf) <israel.alberto.rv@gmail.com>" [ultimate]
```

Both pass with the same workflow configuration.

### D-4 — Parallel job, not a step inside `verify-tag-reachability`

The two guards answer different questions:

- **Reachability**: does the tag point at a commit on `main`?
- **Signature**: did a trusted human cut the tag?

Splitting them into two jobs:

1. Makes the GitHub Actions UI self-documenting (two jobs with
   distinct display names, both visible in the required-checks
   list of the PR / release run).
2. Lets either guard fail independently — the failure log is
   shorter and the remediation step is unambiguous.
3. Keeps each job's runtime budget honest (5 minutes each, both
   must complete before `build-release` proceeds, no shared
   state to leak between them).

The cost is +1 minute of wall-clock when the jobs run in parallel
(negligible — the workflow's critical path is the 5-minute
`build-release` job regardless).

### D-5 — Existing `required_signatures` rule is untouched

The ruleset already has `required_signatures` enforcing commit
signing on `main`. This ADR does **not** amend the ruleset. The
`required_signatures` rule covers `push` events to `main` and
verifies commit signatures; it does **not** cover tag-push events
to refs like `refs/tags/vX.Y.Z`. GitHub's ruleset API exposes
`required_signatures` only as a branch-protection rule, not as a
tag-push hook. The `verify-tag-signature` job is the
mechanism that fills the gap.

## Consequences

### Positive

- **Release builds are gated on tag authenticity.** An unsigned or
  wrongly-signed tag fails the workflow before any binary is
  built. The `verify-tag-reachability` job alone does not provide
  this guarantee.
- **Both signing formats continue to verify.** The maintainer's
  GPG RSA key (v0.13.x and earlier) and ED25519 SSH key
  (v0.14.0+) both work without workflow changes. A future switch
  back to GPG (or to a co-maintainer's key) needs only an allow-list
  update, not a workflow change.
- **The allow-list is auditable.** `.github/trusted-signers` and
  `.github/trusted-signers.asc` are version-controlled, reviewed
  via the `protect-main` ruleset's `require_code_owner_review`
  rule, and visible in `git log -- .github/trusted-signers*`.
- **No secret-management ceremony.** Public keys are public; the
  runner image has no keys to manage, the secrets store has no
  keys to rotate.

### Negative / accepted risks

- **The allow-list must be kept current.** A new maintainer's
  first tag push will fail with "tag is not signed by a key in
  .github/trusted-signers" until the maintainer opens a PR adding
  their key. The failure is informative (the workflow log points
  at `.github/trusted-signers` and the fix steps); the latency is
  one PR.
- **The allow-list is committed, not secret.** A reviewer
  following `git log -- .github/trusted-signers` can see every
  signing key the project has ever trusted. This is intentional —
  the trust boundary is the repository, not a secret store — and
  matches the procedural rule "every signer is documented in
  `.github/CONTRIBUTING.md`".
- **The CI runner's keyring is ephemeral.** The PGP key is
  imported on every run; nothing persists between runs. A
  compromise of the runner image could swap the imported key for
  a forged one. The accepted risk is identical to the existing
  `cargo build --release --locked` step (also runs in a fresh
  runner), and is mitigated by the ruleset's
  `required_status_checks` chain.
- **`git verify-tag` accepts `G` (good+trusted) and `U`
  (good+untrusted) results.** An imported PGP key is `[unknown]`
  in the runner's keyring (no Web of Trust path on a fresh image),
  so verification reports `U` rather than `G`. `git`'s
  `verify_gpg_signed_buffer` (`gpg-interface.c`) reads
  `[GNUPG:] GOODSIG` from gpg's `--status-fd=1` output and exits
  0 in either case. Explicit `gpg --trust-model always` would
  downgrade this to `G` but is **not required**; the
  `verify-tag-signature` job is gated on exit code, not trust
  level. Setting `gpg.minTrustLevel` to anything other than the
  default (`undefined`) would break this job — do not add it.
- **The single-maintainer control gap.** The ruleset
  `protect-main` has `require_code_owner_review: true` but
  `required_approving_review_count: 0`, so a CODEOWNER review is
  auto-requested but not blocking — the sole maintainer merges
  their own PR to `.github/trusted-signers*`. The procedural
  rules in AGENTS.md §"Tag signature guard" are the load-bearing
  control while the repo remains single-maintainer. Re-evaluate
  when a co-maintainer joins (§"Re-evaluation §1").

### Compliance

| Surface | Where it lives | Enforced by |
|---|---|---|
| `validate-tag-input` job | `.github/workflows/release.yml` | regex check on `inputs.tag` + `needs:` chain (every downstream job waits on it) |
| `verify-tag-reachability` job | same | the job itself + pinned SHA output consumed by `build-release` |
| `verify-tag-signature` job | same | the job itself + `build-release` `needs:` it; allow-list fetched from `origin/main` |
| Pinned checkout SHA | same | `build-release` `ref:` set to the immutable commit SHA resolved by `verify-tag-reachability`; tag force-push mid-run cannot redirect it |
| `.github/trusted-signers` | repo root under `.github/` | ruleset `protect-main` `require_code_owner_review` (auto-requests review but is not blocking with the current `required_approving_review_count: 0`) — AGENTS.md §"Tag signature guard" is the load-bearing control |
| `.github/trusted-signers.asc` | repo root under `.github/` | same; only consulted when the tag is PGP-signed |
| `git verify-tag ${TAG}` | same | the only authoritative check; job fails fast on `non-zero` |
| AGENTS.md §"Tag signature guard" | `AGENTS.md` | manual review |
| CONTRIBUTING.md §"Releases" | `.github/CONTRIBUTING.md` | manual review |
| CHANGELOG.md entry | `CHANGELOG.md` `[Unreleased]` | manual review (the PR template's "Closes #N" check + reviewer) |
| Gauntlet mirror | `scripts/gauntlet.sh` step 8 | `SKIP_CLIPPY=1 SKIP_SMOKE=1 scripts/gauntlet.sh` (per-invocation `git -c …` overrides; never writes `.git/config` or `~/.gnupg`) |

## Re-evaluation

This ADR will be revisited when any of the following happen:

1. **A co-maintainer joins and adds a second signing key.** The
   PR must add a line to `.github/trusted-signers` (and, if GPG,
   `.github/trusted-signers.asc`), document the key fingerprint
   in `.github/CONTRIBUTING.md`, and link the maintainer PR in the
   AGENTS.md §"Tag signature guard" section. Re-evaluate whether
   the in-repo allow-list needs a secret-backed tier (e.g. for
   organization-level service-account keys) — the answer is
   likely "no" because public keys are public, but the question
   is worth re-asking once more than one key is in play.
2. **GitHub's ruleset API gains tag-signature enforcement.** If a
   future ruleset rule covers `required_signatures` on tag-push
   events (analogous to the branch protection version), this job
   can be retired and the ruleset becomes the source of truth.
   Track via `gh issue`; re-vote the ADR when the API ships.
3. **The maintainer rotates their signing key.** The rotation
   sequence is: add the new key to `.github/trusted-signers`
   (one PR), cut one release signed with the old key, then
   remove the old key in a follow-up PR after the old-key-signed
   release is at least one minor version old. The "remove after a
   minor version" rule is what `AGENTS.md §"Tag signature guard"`
   already codifies; this ADR does not amend it.
4. **A tag is cut unsigned by accident.** The job's failure log
   spells out the fix steps; the procedural rule
   "`git tag -s vX.Y.Z`" already covers correct operation. If
   this happens more than once a year, re-evaluate whether the
   procedural rule needs a `lefthook` pre-push hook that runs
   `git verify-tag <tag>` locally before allowing `git push`.

Until then, the verdicts above are authoritative.

---

## Appendix A — Why not amend the ruleset instead

GitHub's `required_signatures` rule (in the `protect-main` ruleset,
id `19743104`) enforces commit signing on `main`. It does **not**
have a counterpart for tag-push events. The GitHub ruleset API
exposes `required_signatures` only as a branch-protection rule
(see the [`ruleset` schema
reference](https://docs.github.com/en/rest/rulesets)). Tag
authentication is not surfaced as a first-class concept in the API as
of 2026-09-02.

The `verify-tag-signature` job is the closest structural equivalent
until/unless GitHub ships a native rule. The ADR is
forward-compatible: if GitHub later adds a tag-signature rule, the
job can be retired by setting `if: false` and the ruleset becomes
the source of truth, with no release-flow changes.

## Appendix B — Cross-references back to current sources

- The runner's `gpg.format ssh` config is set at
  `.github/workflows/release.yml` in the new
  `verify-tag-signature` job's "Configure git to use the trusted
  signers file" step.
- The PGP key import is at the same file's "Import GPG allow-list
  if the tag is PGP-signed" step (only runs when the tag carries a
  `BEGIN PGP SIGNATURE` block; for v0.13.4+ SSH-signed tags the
  step is a no-op).
- The allow-list is fetched from `origin/main` via `git show
  "origin/main:.github/trusted-signers"` and
  `git show "origin/main:.github/trusted-signers.asc"`, written to
  `$RUNNER_TEMP/trusted/`, and removed with the runner at run end.
- `.github/trusted-signers` contains the maintainer's SSH
  ED25519 key (fingerprint
  `SHA256:POu2Sr8ILb1IM05Vh1cGU3xivjx05QjWoWYhdLc6YHA`,
  principal `israel.alberto.rv@gmail.com`).
- `.github/trusted-signers.asc` is the public key block exported
  from the maintainer's primary PGP key (long ID `414687A3CD7E65B9`,
  full fingerprint `82DE44111B30F91F55BCEB1F414687A3CD7E65B9`).
- The `validate-tag-input` job (regex `^v[0-9]+\.[0-9]+\.[0-9]+$`)
  and the pinned `build-release` checkout (the immutable
  `tag_commit` output of `verify-tag-reachability`) are the
  TOCTOU closure.
- AGENTS.md §"Tag signature guard" and
  `.github/CONTRIBUTING.md` §"Releases" both link back to this
  ADR. The CHANGELOG `[Unreleased]` entry closes #716.
- The gauntlet mirror is `scripts/gauntlet.sh` step 8 (per-
  invocation `git -c gpg.format=ssh -c
  gpg.ssh.allowedsignersfile=…` overrides; temp `GNUPGHOME` for
  the PGP keyring so neither `.git/config` nor `~/.gnupg` is
  mutated).