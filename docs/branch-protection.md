# Branch protection — moagan

This document describes the GitHub branch-protection rules that `main` should
have, and gives the exact `gh api` commands to apply them.

The rules were designed against the validation tiers in
[`validation-tiers.md`](validation-tiers.md). They are the "monitor where
everything is green" the team uses to know whether `main` is safe to ship from.

## Why

Without branch protection, anyone with write access can merge a PR with red
CI. With it, GitHub disables the **Merge** button until:

- all required status checks pass,
- the branch is up to date with `main`,
- the merge is signed / not a fast-forward (depending on policy), and
- the right number of approving reviews is recorded.

This is the **last** safety net behind T0+T1+T2 (local) and the GitHub
runner (CI).

## Current state

`airvzxf/moagan` is public, has secret-scanning and dependabot alerts enabled,
but `main` has **no** branch protection rules at the time of writing
(verified via `gh api /repos/airvzxf/moagan/branches/main/protection` →
HTTP 404).

## What to enable

The recommended rules for `main`:

| Rule | Value | Why |
|---|---|---|
| Require a pull request before merging | ✅ | No direct pushes; everything reviewed. |
| Required approvals | 1 | Solo dev still benefits from the PR record and CI gate. Bump to N for a team. |
| Dismiss stale pull request approvals when new commits are pushed | ✅ | An approval on commit A does not cover commit A'. |
| Require status checks to pass before merging | ✅ | The CI jobs must be green. |
| Required checks | `lint-test-build`, `smoke-e2e` | The two jobs in `.github/workflows/ci.yml`. |
| Require branches to be up to date before merging | ✅ | Rebase or merge `main` before merging the PR. |
| Require linear history | ✅ | Squash-merge only; no merge commits. |
| Require signed commits | ✅ | Already enforced by global `commit.gpgsign = true`; this makes GitHub verify the signature on push. |
| Do not allow force pushes | ✅ | Prevents rewriting signed history. |
| Do not allow deletions | ✅ | Prevents deleting `main`. |
| Allow auto-merge | optional | If you want Renovate / Dependabot to auto-merge green PRs. |

## Apply it — copy-paste block

Run these once from the repo root. They require `admin` permission on the
repo (you have it; verified by `gh-project-classify.sh` showing
`permission: ADMIN`).

```bash
# Confirm admin permission
gh api /repos/airvzxf/moagan  --jq .permissions

# Apply branch protection. This is a single PUT/POST to the protection
# endpoint with all rules in one shot.
gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  /repos/airvzxf/moagan/branches/main/protection \
  --input - <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": ["lint-test-build", "smoke-e2e"]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": false,
    "required_approving_review_count": 1,
    "require_last_push_approval": false
  },
  "restrictions": null,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "block_creations": false,
  "required_conversation_resolution": true,
  "lock_branch": false,
  "allow_fork_syncing": false,
  "signatures_required": true
}
JSON
```

Then verify:

```bash
gh api /repos/airvzxf/moagan/branches/main/protection \
  --jq '{required_status_checks, required_pull_request_reviews, required_linear_history, allow_force_pushes, allow_deletions, signatures_required}'
```

Expected output (truncated):

```json
{
  "required_status_checks": {
    "strict": true,
    "contexts": ["lint-test-build", "smoke-e2e"]
  },
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "required_approving_review_count": 1
  },
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "signatures_required": true
}
```

## When to update

Re-run the PUT whenever:

- A new required status check is added to `.github/workflows/ci.yml` (add it to
  `required_status_checks.contexts`).
- The CI jobs are renamed (the `contexts` array must match the new names — they
  are case-sensitive and are the `jobs.<id>` key in the workflow YAML, not the
  `name:` field).
- You move from solo to team (bump `required_approving_review_count`).

## Status badge

The README badge is wired against `ci.yml`:

```markdown
[![ci](https://github.com/airvzxf/moagan/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/airvzxf/moagan/actions/workflows/ci.yml)
```

If a job name in `ci.yml` is renamed, the badge URL does not change (the
badge reflects the workflow file, not individual jobs). The branch protection
rules, however, **do** depend on the job IDs — keep them in sync.
