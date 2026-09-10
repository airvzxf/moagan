# ADR 0008 — CI uses `minimax` as the sole real-LLM upstream

> **Status**: Accepted
> **Date**: 2026-09-10
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: the implicit "OpenCode + DeepSeek + MiniMax"
> CI multi-provider stance inherited from
> [`docs/pending-items-2026-08-13.md`](../../docs/cluster-v0.15.0-validation-reports/validate-cluster-scope.md)
> (the prior pending-items file was retired in the 2026-08-28
> docs prune; the opencode/deepseek CI coverage it tracked is
> closed by this ADR).
> **Relates to**:
> [EPIC #851](https://github.com/airvzxf/moagan/issues/851)
> (chore(ci): drop OpenCode+DeepSeek from CI infrastructure),
> [`AGENTS.md` §"Smoke gates"](../../AGENTS.md),
> [`scripts/e2e_audit_proxy.sh`](../../scripts/e2e_audit_proxy.sh)
> (the surviving real-proxy audit harness, post-cleanup),
> [PR #816](https://github.com/airvzxf/moagan/pull/816)
> (the precedent commit — dropped OpenCode workflows and
> integration tests in advance of this ADR).

## Context

Until v0.15.x, the `moagan` CI infrastructure assumed that three
distinct upstream providers would each cover a slice of the
audit surface:

- `minimax` (Anthropic-compatible wire, `/v1/messages`) — the
  primary smoke target.
- `opencode` (`https://opencode.ai/zen/go/v1`) — the operator's
  relay for six other models on two other wire formats
  (`/v1/chat/completions`, `/v1/responses`).
- `deepseek` (`https://api.deepseek.com/v1`, native provider) —
  exercised through its `deepseek-v4-flash` model alias.

Each provider had its own gated section in
`scripts/e2e_audit_proxy.sh` (A.bis, A.ter, A.quad), its own
`--ignored` post-merge workflow
(`test-ignored-{opencode,deepseek,minimax}.yml`), and (for
OpenCode/DeepSeek) its own discovery workflow
(`e2e-network-discover-{opencode,opencode-models,deepseek}.yml`).
The CI orchestration in `e2e-network.yml` documented all of these
in extensive comment blocks.

By v0.15.1 (2026-09-07) the operator's actual usage had drifted:
the OpenCode subscription was cancelled and PR #816 deleted the
OpenCode push triggers and integration tests; the DeepSeek
pay-as-you-go budget had been exhausted and restored (see the
v0.12.x §9.3 breadcrumb) but the per-provider discovery jobs
remained in the harness as disabled stubs.

EPIC #851 (2026-09-09) made the cleanup explicit. This ADR
formalises the resulting policy: **CI uses `minimax` as the sole
real-LLM upstream**. OpenCode and DeepSeek are out of CI scope.
The binary remains provider-agnostic (the `LlmClient` trait
refactor tracked in EPIC #847 preserves that surface), so an
operator who still needs those providers can configure them via
their local `~/.config/moagan/config.toml` and exercise them
outside CI.

## Decision

### 1. CI surface is minimax-only

- `scripts/e2e_audit_proxy.sh` SECTION A retains only the three
  minimax-gated sub-blocks: `card80`, `fast`, `explore`.
- `.github/workflows/test-ignored-minimax.yml` is the sole
  remaining real-provider `--ignored` workflow.
- No per-provider `e2e-network-discover-*.yml` workflows exist.
- The `OPENCODE_API_KEY` and `DEEPSEEK_API_KEY` secrets are no
  longer consumed by any workflow in this repo. They may still be
  loaded by the binary at runtime from the user's environment
  (per `src/llm/api_keys.rs`), but CI does not inject them.

### 2. Test-group manifest is the single source of truth

The surviving test groups (card80, fast, explore) are documented
in a new `declare_test_groups()` function inside
`scripts/e2e_audit_proxy.sh`. The function emits a markdown table
when `MOAGAN_PRINT_TEST_GROUPS=1` is set; this is the input the
docgen `test-skips` subcommand consumes for Layer 6 of
`docs/test-skips-report.md` (closes #856, see EPIC #852).

### 3. `config.example.toml` is unchanged

The operator's template still lists OpenCode and DeepSeek as
configurable providers. CI does not exercise them; the binary
remains provider-agnostic. Operators who maintain those local
configurations are not broken by this ADR.

### 4. Historical references are pruned, not rewritten

- `tests/integration_discover_minimax.rs` header comment
  (companion to `integration_discover_deepseek.rs`) rewritten.
- `.github/workflows/ci.yml` comment block (which listed
  `test-ignored-{deepseek,opencode,minimax}`) rewritten.
- `.github/workflows/e2e-network.yml` comment block rewritten.
- `.github/workflows/test-ignored-minimax.yml` comment block
  rewritten.
- `docs/branch-protection.md` §"Per-provider workflows" rewritten.
- `docs/test-skips.md` (443 LOC) was deleted under EPIC #852 (#869)
  and replaced by the auto-generated `docs/test-skips-report.md`
  (produced by `moagan-docgen test-skips`). Historical ADR
  references to the deleted file are superseded.

### 5. Cluster validation is captured per minor

The MiniMax-only cluster validation report for v0.16.0 lives at
`docs/cluster-v0.16.0-validation-reports/validate-minimax-only-after-cleanup.md`
(written as part of #865; lands in a follow-up commit so the
report can cite the actual end-to-end run).

## Consequences

### Positive

- **One billable upstream.** MiniMax-M3 is the only upstream CI
  talks to. Token spend is auditable in one place
  (`telemetry/calls.jsonl.gz` per run).
- **No stale stubs.** The "disabled stub" class of workflow
  (`test-ignored-deepseek.yml` and friends) is gone; future
  contributors cannot mistake a stub for an active job.
- **Cleaner CI comments.** The 50+ lines of
  `e2e-network.yml` historical breadcrumbs about per-provider
  discovery are reduced to one paragraph pointing at this ADR.
- **Cache footprint drops.** Three of the `e2e-network.yml`
  per-job cache entries (one per provider) are eliminated; the
  surviving set is documented in EPIC #822.
- **Test-group manifest.** The `declare_test_groups()` function
  is the single source of truth for "what does this audit
  exercise", consumable by docgen and by operator dashboards
  without parsing bash.

### Negative

- **OpenCode and DeepSeek have no CI coverage.** Operators who
  rely on those providers locally must validate them out-of-band
  (their own scripts, paid CI, or `make e2e` with the env vars
  exported).
- **Wire-format coverage is reduced to the MiniMax wire.** The
  `/v1/responses` and `/v1/chat/completions` shapes are no longer
  exercised by CI; only the Anthropic-compatible `/v1/messages`
  wire of `minimax` is. If the `LlmClient` refactor (EPIC #847)
  regresses in a way that breaks the other wire shapes, CI will
  not catch it — only the operator's local runs will.
- **Local OpenCode/DeepSeek configs must be opt-in.** An operator
  who previously relied on CI for "free" OpenCode coverage loses
  that signal.

### Mitigations

- The `LlmClient` trait refactor (EPIC #847) preserves the SDK
  per-wire surface; proptest coverage at `src/llm/wire.rs:333-776`
  exercises the wire-body serialisation for all three shapes.
- The `post-release-validation.yml` workflow (added in PR #819)
  runs against the `MINIMAX_API_KEY` and is the canonical
  post-release smoke.
- The `e2e-network-card80.yml` workflow remains as a manual
  dispatch for the long-form card80 run; an operator can fork
  the pattern to add a manual-only OpenCode/DeepSeek card80 if
  they need it locally.

## Alternatives considered

### A. Restore OpenCode coverage as a manual workflow

Rejected. The operator already cancelled the OpenCode
subscription; there is no upstream to call. A workflow that
permanently skips would be a worse failure mode than not having
the workflow at all (silent pass on a missing secret is exactly
the bug class `MINIMAX_API_KEY`-gated workflows avoid via the
explicit `if [[ -n "${MINIMAX_API_KEY:-}" ]]; then … SKIP … fi`
block).

### B. Keep `test-ignored-deepseek.yml` as a manual dispatch

Rejected. The DeepSeek budget has been restored, but the
operator's CI practice no longer exercises per-provider
discovery — the MiniMax `--ignored` test (card80 discover) is
the canonical regression surface. Re-adding a manual-only
DeepSeek workflow would invite the next operator to assume CI
coverage that does not exist.

### C. Add a `mock-deepseek` and `mock-opencode` provider kind for CI

Out of scope for this ADR but a reasonable future direction. The
`LlmClient` refactor (EPIC #847) introduces a `MockClient`; once
the SDK impls for Anthropic, OpenAI Chat, and OpenAI Responses
are stable, the existing wiremock fixtures can be re-purposed
to exercise non-MiniMax wire shapes without a real upstream.
Filed as a follow-up under #847's scope.

## References

- EPIC #851 (chore(ci): drop OpenCode+DeepSeek from CI
  infrastructure) — the action plan this ADR formalises.
- PR #816 (chore(deps): drop opencode subscription workflows and
  integration test) — the precedent commit (2026-09-08).
- PR #819 (ci(workflows): add post-release-validation workflow) —
  the canonical post-release smoke.
- ADR-0006 (Structural validation for
  `integration_discover_minimax`) — the structural-validation
  test design that this ADR's surviving test relies on.
- ADR-0007 (Moagan is a single-crate monolith by default) — the
  scope-of-the-repo decision that lets one CI surface cover the
  whole binary.
- `docs/cluster-v0.15.0-validation-reports/validate-cluster-scope.md:25-31`
  — the cluster scope analysis that motivated this ADR.
- `AGENTS.md` §"Smoke gates" — the operator-facing rule that
  "CI uses minimax only" formalises.
