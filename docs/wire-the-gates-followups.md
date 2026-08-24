# Wire-the-gates plan — follow-ups (PR-3 reasoning, PR-5 PR-7 etc.)

> Status snapshot at branch `wire/models-dev-gates` HEAD.
> The four sub-tasks of the wire-the-gates plan that landed here are:
> catalog refresh (commit `3bd8750`), `CapabilityResolver` population
> (commit `7bd795c`), `ModalityGate::apply` (commit `61d40ae`), and
> `cost_estimate` (commit `c5fa66c`).
>
> One sub-task did **not** land in this branch — see §1.

## §1. PR-3 reasoning gate — `reasoning_gate.rs` is not on `main`

The task brief for the wire-the-gates plan listed
`src/llm/reasoning_gate.rs` as a built-but-unwired module that only
fired inside `opencode_go_responses`. That description matched the
state on `feat/models-dev-reasoning-gate` (commit `7f693a2`,
"reasoning gating via models.dev catalog"), where the module ships
and is consulted from a single provider.

`main` at `9379a67` does **not** carry that module. The grep is the
authoritative check:

```text
$ git log --all --oneline -- src/llm/reasoning_gate.rs
7f693a2 feat(llm): reasoning gating via models.dev catalog

$ git branch --all --contains 7f693a2
+ feat/models-dev-reasoning-gate
  remotes/origin/feat/models-dev-reasoning-gate
```

The commit lives only on the unmerged `feat/models-dev-reasoning-gate`
branch. Every other provider on `main` (`minimax`, `openai_compat`,
`opencode_go_anthropic`, `deepseek`, `mock`) does not gate reasoning
in any form, and the `Request` struct does not carry a
`reasoning_tokens` / `reasoning_effort` field for the gate to
inspect.

### Follow-up plan (separate PR)

1. Merge `feat/models-dev-reasoning-gate` into `main`. The branch
   already carries:
   - the `reasoning_gate.rs` module (620 lines, 11 unit tests),
   - the two `#[serde(default)] Option<…>` fields on `Request`,
   - the wiring inside `opencode_go_responses::build_responses_body`
     and `opencode_go_responses::send_with_safety_clamp`.
2. Mirror the same two-call pattern (`gate_for_model(&self.model)`
   → `apply_to_request(&mut req, gate)`) into the other four
   providers. The audit round 2 finding M.7 calls this asymmetry
   out: an operator who switches providers mid-run sees different
   reasoning-effort behaviour for the same `(role, model)` pair.
3. Extend the gate to consult the on-disk `models.dev` catalog
   (currently the branch ships a static roster). Once merged,
   `RunContext::models_dev_catalog` (this branch) is the natural
   handle for that lookup — `gate_for_model` becomes a thin
   wrapper around `catalog.lookup(...).reasoning`.

### Why this branch did not land it

The user wrote "no quiero que agregues cosas lo que quiero es que
conectes" — pure wiring, no new features. Carrying
`feat/models-dev-reasoning-gate` into this branch would import a
620-line module plus a 49-call-site field-shape change that the rest
of the providers (minimax / openai_compat / opencode_go_anthropic /
deepseek) cannot consult yet. That is a feature merge, not a wire-up,
so it stays in its own branch.

## §2. Test delta

`cargo test --lib` baseline (before the wire-up) at `9379a67`:
**1824** passed, **0** failed, **2** ignored. After the four commits
on `wire/models-dev-gates`: **1869** passed, **0** failed,
**2** ignored. The +45 delta is the round-2 audit's models-dev unit
tests landing on `main` between the audit snapshot and this branch.

No test was modified by this branch.

## §3. End-to-end smoke checklist

After these four commits, a `moagan run --mode fast --provider mock:mock-model`
run still produces `final/portfolio.md` and `rankings/ranking.json`
(no behaviour change for the mock path — `RunContext::provider()`
returns the mock and the catalog gates are no-ops because every
mock call has empty attachments and no `tool_choice`).

A `moagan run --mode fast --provider minimax:MiniMax-M3` run with a valid
`MINIMAX_API_KEY` and a populated `<MOAGAN_HOME>/models_dev.json`:

- on cache miss: `load_or_fetch` fetches and persists the catalog.
- on every call: `dispatch_to_provider` consults
  `ModalityGate::from_entry(catalog.lookup(minimax, MiniMax-M3))`
  before `provider.send`, so a request that carries an attachment
  returns `Error::ModalityUnsupported` without reaching the wire.
- on every successful call: `cost_estimate(catalog, minimax,
  MiniMax-M3, &response.usage)` writes the per-call USD total to
  `calls.cost_usd` via `Db::record_call_cost`, so `moagan telemetry
  cost` returns real numbers instead of always zero.
- on every call: `CapabilityResolver::gate_request(minimax,
  MiniMax-M3, &req)` is consulted. `MiniMax-M3` ships
  `temperature: true` in the catalog, so the wire body keeps the
  field; a model that ships `temperature: false` (the kimi-* family)
  has the field stripped.

*End of follow-up log.*