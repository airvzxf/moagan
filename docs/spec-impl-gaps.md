# Spec vs implementation — directory gaps

> **Scope**: cross-check between `docs/proposal-02-rust.md` §0.2
> "Layout de directorios (código fuente)" (spec id `T01-06`) and
> the actual `src/` tree at HEAD `0411ffd` (post `fix/audit-findings-2`).
> **Source of truth**: `docs/proposal-02-rust.md` lines 25–160.

The 2026-08-12 codebase audit (`docs/inconsistencies-audit-2026-08-12.md`
§D.1, work-tree only) flagged that the spec called for several files
that do not exist on disk. This document records, for each gap,
**what the spec asked for**, **what the implementation actually did**,
and **whether the gap is intentional** (collapsed layout) or **still
unresolved** (missing capability).

## TL;DR

| Spec asked for | On disk? | Resolution |
|---|---|---|
| `src/ingest/{mod,normalize,detect,budget}.rs` | No | Folded into `phases/intake.rs`. Intentional collapse. |
| `src/llm/retry.rs` | No | Split between `llm/retry_budget.rs` (RetryReason table) and `phases/phase.rs::call_with_retry_parse` (the canonical retry loop). Intentional split. |
| `src/llm/glm.rs` | No | No GLM provider ships in v0.x. **Open gap** — would require a new provider module + config block + role catalog entry. |
| `src/llm/qwen.rs` | No | No Qwen provider ships in v0.x. **Open gap**. |
| `src/llm/kimi.rs` | No | No Kimi provider ships in v0.x. **Open gap**. |
| `src/domain/{run,brief,sketch,proposal,critique,validation,evaluation,tag,cluster,facet,contradiction,matrix}.rs` | No | Single `src/domain/mod.rs` (2,323 lines) plus `constraint.rs`, `graph.rs`, `synthesis_request.rs`. Intentional collapse — the per-type files never materialised. |
| `src/discovery/{tagger,clusterer,contradiction,facet,extractor,integrator}.rs` | Partial | The discovery modes exist; the helpers live inside `src/phases/discover_*.rs` and `src/discovery/{coordinator,persona_angle,stop_policy,...}.rs`. Intentional collapse. |

Three of the seven items are **still open**: GLM, Qwen, and Kimi
providers. None have been requested by an operator; adding any one
requires a Cargo dep, a `Provider` impl, an HTTP-wire module, a role
catalog entry for the API style, and a smoke probe. Closing them is
out of scope for the audit cleanup pass.

## 1. `src/ingest/` — collapsed into `phases/intake.rs`

**Spec (T01-06 §0.2 lines 59–63):**

```text
ingest/
├── mod.rs
├── normalize.rs
├── detect.rs
└── budget.rs
```

**Impl:** no `src/ingest/` directory exists. The four responsibilities
landed in `src/phases/intake.rs` as free functions and an enum:

- `normalize` → `phases::intake::normalize_raw_prompt` (line 564).
  Strips BOM, collapses whitespace, truncates to
  `llm::size_limits::MAX_PROMPT_BYTES` (D.29.2, 250 KiB), returns
  the cleaned prompt.
- `detect` → `phases::intake::HostilePolicy` + `HeuristicOutcome` +
  `run_hostile_detector` (line 421). Two-stage detector:
  heuristic then LLM (`Role::HostilePromptDetector`).
- `budget` → `Config::token_budget` field, fed into `Db::set_budget`
  by `cli/run.rs` (wired in commit `e8b682f`). Token-side budget
  accounting lives in `llm/budget.rs` and `phases/budget.rs`.
- `mod` → `phases/intake.rs::IntakePhase` is the orchestrator.

**Verdict:** intentional collapse. The four concerns share the same
`RunContext` and the same LLM call machinery; splitting them into
`src/ingest/` would force three layers of pub use re-exports for no
behavioural gain. No future PR plans to revive the directory.

## 2. `src/llm/retry.rs` — split into `retry_budget.rs` + `phases/phase.rs`

**Spec (T01-06 §0.2 line 68):**

```text
llm/
└── retry.rs
```

**Impl:** no `src/llm/retry.rs` exists. The retry concern is split
across two files:

- `src/llm/retry_budget.rs` — the **classifier**. `RetryReason`
  enum (Timeout, RateLimit, Schema, Transport, Truncated, …) plus
  `budget_for(mode, reason) -> RetryBudget` and
  `reason_from_error(err) -> RetryReason`. Used by every provider
  to decide whether to back off and how many attempts to allow.
- `src/phases/phase.rs::call_with_retry_parse` — the **loop**.
  `parse_model_json` + `validate_json(role)` + role-budget clamp +
  audit hash + telemetry. This is the single chokepoint every LLM
  call funnels through.

The split is deliberate: `retry_budget.rs` is pure data
(`#[derive]` + simple matchers, no async) and `phases/phase.rs`
holds the side-effecting loop. Merging them back into a single
`llm/retry.rs` would force `phases/` to depend on `tokio` /
`tracing` / `serde_json::Value` constructors in a way the type
boundary today prevents.

**Verdict:** intentional split. No future PR plans to merge.

## 3. `src/llm/{glm,qwen,kimi}.rs` — open gaps

**Spec (T01-06 §0.2 lines 73–76):**

```text
llm/
├── glm.rs        ← MISSING
├── qwen.rs       ← MISSING
└── kimi.rs       ← MISSING
```

**Impl:** none of these provider modules exist. The current provider
surface is six modules:

| Provider | File | HTTP style |
|---|---|---|
| DeepSeek | `src/llm/deepseek.rs` | OpenAI Chat Completions |
| MiniMax | `src/llm/minimax.rs` | Anthropic-compatible messages |
| OpenCode Go (Anthropic) | `src/llm/opencode_go_anthropic.rs` | Anthropic-compatible messages |
| OpenCode Go (Responses) | `src/llm/opencode_go_responses.rs` | OpenAI Responses API |
| OpenCode Go (generic) | `src/llm/opencode_go.rs` | façade |
| Mock | `src/llm/mock.rs` | in-process canned responses |

**Verdict:** **open gap**. To add any of GLM / Qwen / Kimi, a PR would
need to:

1. Add a Cargo dep on the provider SDK or pin a raw-HTTP schema
   (the latter is the moagan house style — see
   `docs/proposal-02-rust.md` §0.1 "no Anthropic SDK" guard).
2. Write a new `src/llm/<name>.rs` implementing the `Provider`
   trait (`src/llm/provider.rs`).
3. Wire the provider into `src/llm/provider_pool.rs` and
   `src/llm/capabilities.rs` (per-provider `MAX_TOKENS_CAP`
   constant goes here, see audit §C.5 row 4).
4. Add a smoke gate to `docs/validation-tiers.md` so CI exercises
   the new wire (the auto-probe feature from v0.7 covers DeepSeek /
   MiniMax / OpenCode Go via `src/llm/probe.rs`).

Until an operator files a ticket, none of these three are planned.

## 4. `src/domain/<single-type>.rs` — collapsed into `mod.rs`

**Spec (T01-06 §0.2 lines 44–58):**

```text
domain/
├── run.rs, brief.rs, sketch.rs, proposal.rs, critique.rs,
├── validation.rs, evaluation.rs, tag.rs, cluster.rs,
├── facet.rs, contradiction.rs, matrix.rs
```

**Impl:** `src/domain/` contains exactly four files:

| File | Lines | Purpose |
|---|---|---|
| `src/domain/mod.rs` | 2,284 | Every domain struct the spec wanted as separate files, plus the per-mode dispatch types. |
| `src/domain/constraint.rs` | ~200 | `HARD_INCOMPATIBILITIES` + `is_incompatible` + the constraint set iterated by `phases/synthesize.rs`. |
| `src/domain/graph.rs` | ~300 | `ProblemGraph` (decomposer DAG) + `topological_layers`. |
| `src/domain/synthesis_request.rs` | ~150 | The `SynthesizePhase` request envelope. |

**Verdict:** intentional collapse. The original `domain/<type>.rs`
split would have produced 13 files averaging 150 lines each; the
collapsed form puts the cross-referencing types (Brief ↔ Intake ↔
Proposal) in one place. `constraint.rs`, `graph.rs`, and
`synthesis_request.rs` were split out as separate files because they
each carry non-trivial logic (DAG topo sort, compatibility matrix,
synthesis request serialiser), not just struct definitions.

## 5. `src/discovery/<helper>.rs` — partially collapsed

**Spec (T01-06 §0.2 lines 111–119):**

```text
discovery/
├── tagger.rs, clusterer.rs, contradiction.rs, facet.rs,
├── extractor.rs, integrator.rs
```

**Impl:** `src/discovery/` contains the per-helper logic but split
between `src/discovery/` (orchestration + types) and
`src/phases/discover_*.rs` (the phase shells):

| Helper concept | Where it lives |
|---|---|
| Tagger | `src/phases/discover_tag.rs` + `src/domain/mod.rs::SketchTags` |
| Clusterer | `src/discovery/clusterer.rs` + `src/phases/discover_summary.rs` |
| Contradiction | (no production code — deferred per `v0.7-final-report.md` §7) |
| Facet | `src/phases/discover_facet.rs` + `src/domain/mod.rs::FacetList` |
| Extractor | `src/phases/discover_extract.rs` + `src/domain/mod.rs::FacetExtraction` |
| Integrator | `src/phases/discover_integrate.rs` + `src/domain/mod.rs::CategoryDoc` |
| Persona / Angle picker | `src/discovery/persona_angle.rs` |
| Stop policy | `src/discovery/stop_policy.rs` |
| Coordinator | `src/discovery/coordinator.rs` |

**Verdict:** intentional partial collapse. The phase shell stays in
`phases/` (consistent with every other phase); the per-helper data
shapes live next to the orchestrator that owns them. Contradiction
detection is the one helper that never materialised — the `v0.7-final-report`
defers it.

## Cross-references

- `docs/proposal-02-rust.md` §0.2 lines 25–160 — the original layout
  the spec asked for.
- `docs/proposal-02-rust.md` §0.1 — "raw HTTP via reqwest, no
  Anthropic SDK" guard that keeps every provider module
  dependency-free.
- `docs/inconsistencies-audit-2026-08-12.md` §D.1 — the audit
  finding this document closes out (work-tree only; not committed).
- `docs/v0.7-final-report.md` §7 — deferred items (contradiction
  detection, GLM / Qwen / Kimi providers, comfy-table, petgraph,
  proptest, multimodal streaming).

## Future work

If/when GLM / Qwen / Kimi become requested:

1. Open a tracking issue with the target use case (DeepSeek-style
   cheap general model? GLM-4 for CJK prompts? Kimi for long-context
   rollouts of >100 k tokens?).
2. Decide on raw HTTP vs SDK — the project policy is raw HTTP
   (`scripts/check-no-anthropic-sdk.sh` enforces this for the
   Anthropic wire; the same gate should be applied to any new
   provider SDK).
3. Follow the four-step wiring list in §3 above.

For everything else in this document, the spec-vs-impl gap is
**intentional collapse** and not a defect.
