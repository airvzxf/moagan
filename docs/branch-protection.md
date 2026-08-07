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
| `pull_request` | ✓ | Requires a PR before merging. `required_approving_review_count: 0`, `dismiss_stale_reviews_on_push: true`, `required_review_thread_resolution: true`, allowed merge methods: `squash`, `rebase`. |
| `required_linear_history` | ✓ | Enforces linear history. |
| **`required_status_checks`** | ✗ **MISSING** | The CI gate. This is what we add. |

The classic branch-protection endpoint returns HTTP 404
(`/branches/main/protection`) — that endpoint is deprecated in favour of
rulesets. Don't try to apply rules there.

## What we add

A single new rule to the existing ruleset:

| Rule | Value | Why |
|---|---|---|
| `required_status_checks` | `strict: true`, 9 contexts: `fmt-check`, `guard-deps`, `clippy`, `build`, `test-lib`, `test-tests`, `test-doc`, `smoke`, `e2e` | Each of the 9 parallel jobs in `.github/workflows/ci.yml` must be green before merge. `strict: true` forces the PR to be up to date with `main` first. |

The 9 job IDs come from the `jobs.<id>` keys in the workflow YAML (NOT the
`name:` field — those can be human-readable). They are case-sensitive.

`e2e-network` is intentionally NOT in the required list — it runs only
post-merge on `main` (it's the 25-minute real-LLM audit, not a PR gate).

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
jq '.rules += [{
  "type": "required_status_checks",
  "parameters": {
    "strict_required_status_checks_policy": true,
    "required_status_checks": [
      { "context": "fmt-check" },
      { "context": "guard-deps" },
      { "context": "clippy" },
      { "context": "build" },
      { "context": "test-lib" },
      { "context": "test-tests" },
      { "context": "test-doc" },
      { "context": "smoke" },
      { "context": "e2e" }
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
    { "context": "fmt-check" },
    { "context": "guard-deps" },
    { "context": "clippy" },
    { "context": "build" },
    { "context": "test-lib" },
    { "context": "test-tests" },
    { "context": "test-doc" },
    { "context": "smoke" },
    { "context": "e2e" }
  ]
}
```

## When to update

Re-run the GET-modify-PUT cycle whenever:

- A new required status check is added to `.github/workflows/ci.yml` (add it
  to `required_status_checks[].context`).
- A CI job is renamed (the `context` array must match the new job ID — they
  are case-sensitive and are the `jobs.<id>` key in the workflow YAML).
- You move from solo to team (set `required_approving_review_count > 0` on
  the `pull_request` rule).

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