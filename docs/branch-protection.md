# Branch protection — moagan

This document describes the GitHub **ruleset** that protects `main`, and the
exact `gh api` commands to add the `required_status_checks` rule once CI is
green.

The rules are the "monitor where everything is green" the team uses to know
whether `main` is safe to ship from.

## Why

Without branch protection, anyone with write access can merge a PR with red
CI. With it, GitHub disables the **Merge** button until:

- all required status checks pass,
- the PR is up to date with `main` (`strict: true`),
- the merge is `squash` or `rebase` (no merge commits),
- all review threads are resolved.

This is the **last** safety net behind T0+T1+T2 (local hooks) and the GitHub
runner (CI).

## Current state — what you already have

`airvzxf/moagan` is public, has `secret_scanning` and `dependabot_security_updates`
enabled, and protects `main` via a **ruleset** named `protect-main`
(id `19743104`, source `Repository`, enforcement `active`, target
`~DEFAULT_BRANCH`).

Verified via:
```bash
gh api /repos/airvzxf/moagan/rulesets  --jq '.[] | {id,name,target,enforcement}'
gh api /repos/airvzxf/moagan/rulesets/19743104  --jq '{name,target,enforcement,rules:[.rules[].type]}'
```

Existing rules in `protect-main`:

| Rule | Status | What it does |
|---|---|---|
| `deletion` | ✓ | Prevents deleting `main`. |
| `non_fast_forward` | ✓ | Prevents force-pushes to `main`. |
| `pull_request` | ✓ | Requires a PR before merging. `required_approving_review_count: 0`, `dismiss_stale_reviews_on_push: true`, `require_code_owner_review: true`, `require_last_push_approval: true`, `required_review_thread_resolution: true`, allowed merge methods: `squash`, `rebase`. |
| `required_linear_history` | ✓ | Enforces linear history. |
| `required_status_checks` | ✓ | The 9 CI contexts from `ci.yml`. *See [Job IDs vs display names](#job-ids-vs-display-names) below.* |
| `required_signatures` | ✓ | Every commit landing on `main` must be GPG-signed. Last-resort enforcement on top of the local `commit.gpgsign=true` config. |
| **`block_force_pushes`** | ✗ skipped | Redundant with `non_fast_forward`; keep the latter only. |
| **`required_approving_review_count > 0`** | ✗ skipped | Single-maintainer repo. Flip to `1` when co-maintainers are added. |

The classic branch-protection endpoint returns HTTP 404
(`/branches/main/protection`) — that endpoint is deprecated in favour of
rulesets. Don't try to apply rules there.

## What we add

A single new rule to the existing ruleset:

| Rule | Value | Why |
|---|---|---|
| `required_status_checks` | `strict: true`, 9 contexts: `fmt-check`, `guard-deps`, `clippy`, `build`, `test-lib`, `test-tests`, `test-doc`, `smoke`, `e2e` | Each of the 9 parallel jobs in `.github/workflows/ci.yml` must be green before merge. `strict: true` forces the PR to be up to date with `main` first. |

## Job IDs vs display names

The ruleset `required_status_checks` rule uses the human-readable
`name:` of each job as the context string, **not** the `jobs.<id>`
key. The YAML below shows the two layers for each job from
`.github/workflows/ci.yml`:

| Job ID (`jobs.<id>`) | Display name (`name:`) |
|---|---|
| `fmt-check` | `T0 · fmt-check` |
| `guard-deps` | `T0 · guard-deps` |
| `clippy` | `T1 · clippy` |
| `build` | `T1 · build (populates cargo cache)` |
| `test-tests` | `T2 · cargo test --tests (integration)` |
| `test-lib` | `T2 · cargo test --lib --bins` |
| `test-doc` | `T2 · cargo test --doc` |
| `smoke` | `T3 · make smoke (static + 4 pre-existing FAIL)` |
| `e2e` | `T3 · make e2e (local mock pipeline)` |

The `context` strings in the `required_status_checks` JSON block are
the right-hand column. They are case-sensitive and must match what
GitHub renders in the PR's "Checks" tab.

`e2e-network` is intentionally NOT in the required list — it runs only
post-merge on `main` (it's the 25-minute real-LLM audit, not a PR gate).
The new `codeql` and `cargo-audit` workflows are also informational;
they show up as checks but do not block merges.

## Apply it — copy-paste block

Run these once from the repo root. They require `admin` permission on the
repo (you have it).

The rulesets API is **PUT-the-whole-ruleset**: you can't PATCH just one rule.
So we GET the current rules, append the new one, then PUT the whole thing.
This guarantees we don't lose any of the existing rules.

```bash
# 1. Confirm admin permission
gh api /repos/airvzxf/moagan --jq .permissions

# 2. GET current ruleset
gh api /repos/airvzxf/moagan/rulesets/19743104 > /tmp/ruleset.json

# 3. Build the new ruleset JSON: existing rules + new required_status_checks
#
# IMPORTANT: the `context` strings below MUST match the GitHub-reported
# check name exactly, which is the `name:` field of each job in
# .github/workflows/ci.yml (NOT the `jobs.<id>` key). Currently:
#   T0 · fmt-check
#   T0 · guard-deps
#   T1 · clippy
#   T1 · build (populates cargo cache)
#   T2 · cargo test --lib --bins
#   T2 · cargo test --tests (integration)
#   T2 · cargo test --doc
#   T3 · make smoke (static + 4 pre-existing FAIL)
#   T3 · make e2e (local mock pipeline)
# Renaming any of these requires updating this ruleset too, otherwise
# the merge will be blocked with "9 of 9 required status checks are
# expected" (the contexts are tracked by display name).
jq '.rules += [{
  "type": "required_status_checks",
  "parameters": {
    "strict_required_status_checks_policy": true,
    "required_status_checks": [
      { "context": "T0 · fmt-check" },
      { "context": "T0 · guard-deps" },
      { "context": "T1 · clippy" },
      { "context": "T1 · build (populates cargo cache)" },
      { "context": "T2 · cargo test --lib --bins" },
      { "context": "T2 · cargo test --tests (integration)" },
      { "context": "T2 · cargo test --doc" },
      { "context": "T3 · make smoke (static + 4 pre-existing FAIL)" },
      { "context": "T3 · make e2e (local mock pipeline)" }
    ]
  }
}]' /tmp/ruleset.json > /tmp/ruleset-new.json

# 4. PUT the updated ruleset
gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  /repos/airvzxf/moagan/rulesets/19743104 \
  --input /tmp/ruleset-new.json

# 5. Verify
gh api /repos/airvzxf/moagan/rulesets/19743104 \
  --jq '{name, target, enforcement, rules: [.rules[] | .type]}'
```

Expected output after step 5:

```json
{
  "name": "protect-main",
  "target": "branch",
  "enforcement": "active",
  "rules": [
    "deletion",
    "non_fast_forward",
    "pull_request",
    "required_linear_history",
    "required_status_checks"
  ]
}
```

Then check the contexts:

```bash
gh api /repos/airvzxf/moagan/rulesets/19743104 \
  --jq '.rules[] | select(.type == "required_status_checks") | .parameters'
```

Should print:

```json
{
  "strict_required_status_checks_policy": true,
  "required_status_checks": [
    { "context": "T0 · fmt-check" },
    { "context": "T0 · guard-deps" },
    { "context": "T1 · clippy" },
    { "context": "T1 · build (populates cargo cache)" },
    { "context": "T2 · cargo test --lib --bins" },
    { "context": "T2 · cargo test --tests (integration)" },
    { "context": "T2 · cargo test --doc" },
    { "context": "T3 · make smoke (static + 4 pre-existing FAIL)" },
    { "context": "T3 · make e2e (local mock pipeline)" }
  ]
}
```

## When to update

Re-run the GET-modify-PUT cycle whenever:

- A new required status check is added to `.github/workflows/ci.yml` (add it
  to `required_status_checks[].context`).
- A CI job is renamed (the `context` array must match the new display name —
  they are case-sensitive and are the `name:` field of the job, not the
  `jobs.<id>` key; see [Job IDs vs display names](#job-ids-vs-display-names)).
- You move from solo to team (set `required_approving_review_count > 0` on
  the `pull_request` rule, and flip `require_code_owner_review: true` once
  co-maintainers are added to `.github/CODEOWNERS`).
- `.github/CODEOWNERS` is added or its paths change — the ruleset
  `pull_request` rule must be re-PUT to toggle
  `require_code_owner_review` accordingly (see [Optional hardening](#optional-hardeningskip-already-applied--kept-here-for-reference) below).

## Repo-level Actions hardening (applied)

These live on the repo settings, not on the ruleset:

```bash
# 1. SHA-pinning required — every action referenced from a workflow
#    must be pinned to a commit SHA. The legacy tag-style references
#    (e.g. actions/checkout@v4) were already replaced by the
#    ci(workflow) commit in the same dependency-hardening PR.
gh api -X PUT /repos/airvzxf/moagan/actions/permissions \
  -H "Accept: application/vnd.github+json" \
  -H "Content-Type: application/json" \
  --input '{"enabled": true, "allowed_actions": "all", "sha_pinning_required": true}'

# 2. Default workflow token is read-only. A workflow that needs
#    write access must declare it explicitly in the workflow's
#    `permissions:` block.
gh api -X PUT /repos/airvzxf/moagan/actions/permissions/workflow \
  -H "Accept: application/vnd.github+json" \
  -H "Content-Type: application/json" \
  --input '{"default_workflow_permissions": "read", "can_approve_pull_request_reviews": false}'

# 3. Use the PR title as the squash-commit subject and the PR body as
#    the commit body. Combined with `Closes #N` in the template, this
#    preserves the issue link in the squash commit.
gh api -X PATCH /repos/airvzxf/moagan \
  -H "Accept: application/vnd.github+json" \
  -H "Content-Type: application/json" \
  --input '{
    "use_squash_pr_title_as_default": true,
    "squash_merge_commit_title": "PR_TITLE",
    "squash_merge_commit_message": "PR_BODY",
    "delete_branch_on_merge": true,
    "web_commit_signoff_required": true
  }'
```

After these land, the GitHub UI will:

- Reject any workflow whose `uses:` references a tag rather than a SHA.
- Reject any workflow that requests a write scope without declaring it
  in a `permissions:` block.
- Use the PR title as the commit title on every squash merge.
- Delete the source branch automatically after merge.
- Require a web-editor sign-off for commits made via the GitHub web UI.



## Optional hardening (skip / already applied — kept here for reference)

### `required_signatures` — applied

```bash
# 1. GET
gh api /repos/airvzxf/moagan/rulesets/19743104 > /tmp/ruleset.json

# 2. Add required_signatures if missing
jq '
  .rules |= (
    if (map(.type) | index("required_signatures")) then . else . + [{
      "type": "required_signatures"
    }] end
  )
' /tmp/ruleset.json > /tmp/ruleset-new.json

# 3. PUT (after stripping read-only fields: id, node_id, created_at,
#    updated_at, _links, source_type, source, current_user_can_bypass)
gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  /repos/airvzxf/moagan/rulesets/19743104 \
  --input /tmp/ruleset-new.json
```

### `require_code_owner_review` — applied

```bash
# 1. GET
gh api /repos/airvzxf/moagan/rulesets/19743104 > /tmp/ruleset.json

# 2. Flip require_code_owner_review on the pull_request rule
jq '
  .rules |= (
    map(
      if .type == "pull_request" then
        .parameters.require_code_owner_review = true
      else . end
    )
  )
' /tmp/ruleset.json > /tmp/ruleset-new.json

# 3. PUT (same read-only field strip as above)
```

### `required_approving_review_count > 0` — NOT applied

Flip this when co-maintainers are added. With the current single-owner
setup it would block every PR until the owner self-approves, which is
the same as the current behaviour with `require_code_owner_review: true`
minus the dry-run.

## Status badge

The README badge is wired against `ci.yml`:

```markdown
[![ci](https://github.com/airvzxf/moagan/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/airvzxf/moagan/actions/workflows/ci.yml)
```

The badge reflects the workflow file, not individual jobs. The ruleset
`required_status_checks` rule depends on the job IDs — keep them in sync.

## Known issue: pre-existing smoke failures

At the time of writing, `make smoke` fails 4 tests in
`scripts/smoke_intra_cluster_synthesis.sh` (pre-existing on `main`, not
introduced by the tiered-validation refactor):

```
Intra-cluster synthesis smoke tests: PASS=74  FAIL=4
  - synthesize_uses_synthesizer_role
  - synthesize_handles_empty_cluster_list
  - pipeline_synthesized_proposal_has_three_sources
  - pipeline_three_proposals_persisted
```

These are static grep checks, not runtime tests. Once the ruleset
`required_status_checks` rule includes `smoke`, the merge button will be
disabled until these are fixed. They should be fixed in a separate PR before
the ruleset change is applied, OR the `smoke` context should be temporarily
removed from the required list.

Recommended order:

1. Land the tiered-validation refactor PR (this document's companion).
2. Land a separate PR fixing the 4 synthesis smoke tests.
3. Then apply the ruleset change above (with all 9 contexts required).