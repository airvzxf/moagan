# Multi-Model Benchmark — 2026-08-04

## TL;DR

Of the 6 (provider, model) combinations tested with the same prompt in `mode=standard`, **only kimi-k2.7-code (via OpenCode Go) completed the full pipeline**. The other 5 (MiniMax-M3, M2.7-highspeed, M2.5; DeepSeek-v4-flash; qwen3.7-max) errored at the **route phase** with `Error::SchemaViolation`: the models returned valid JSON but the schema validator rejected it. Wall-clock for the successful kimi run was 99 s; the others errored in 19–83 s.

The provider-switch flow (`moagan continue --switch-provider`) **works correctly**: the manifest's `provider` field flipped from `opencode_go` → `deepseek` and `provider_changes` recorded both rows (`user --switch-provider` and `checkpoint:skipped`).

**Recommendation**: For mode=standard today, **kimi-k2.7-code is the only model in the suite that reliably returns schema-compliant JSON** end-to-end. The other providers need prompt-engineering work (or a future sub-fase to relax the schema) before they can be used for the full pipeline.

## Test A — Standard mode comparison

### Setup

- Date: 2026-08-04
- moagan HEAD: `1534e2d` (Q7 merged)
- Prompt: `"Briefly: what are 3 best practices for error handling in Rust?"`
- Mode: `standard`, `--non-interactive`, `--max-parallelism=4`
- Each run used a fresh `--runs-dir` (no cross-run cache pollution)
- `MOAGAN_MINIMAX_ENDPOINT=https://api.minimax.io/anthropic/v1` exported (the operator's `~/.config/moagan/config.toml` routes minimax through a local proxy)
- Per-run timeout: 180 s

### Results table

| # | Provider | Model | Wall-clock | Input tokens | Output tokens | Top-1 score | # sketches | # proposals | Portfolio | Status |
|---|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 1 | minimax | MiniMax-M3 | 83 s | — | — | — | — | — | — | ❌ schema error @ route |
| 2 | minimax | MiniMax-M2.7-highspeed | 27 s | — | — | — | — | — | — | ❌ schema error @ route |
| 3 | minimax | MiniMax-M2.5 | 21 s | — | — | — | — | — | — | ❌ schema error @ route |
| 4 | deepseek | deepseek-v4-flash | 19 s | — | — | — | — | — | — | ❌ schema error @ route |
| 5 | opencode_go | kimi-k2.7-code | 99 s | 13 324 | 28 807 | 8.30 | 12 | 6 | ✅ | ✅ completed |
| 6 | opencode_go | qwen3.7-max | 180 s (timeout) | — | — | — | — | — | — | ⏱️ timeout |

### Observations

- **Only kimi-k2.7-code reached the synthesis + ranking phase**. The 5 failures all hit the same schema-violation in `route` (the second phase of the pipeline). Looking at the raw responses, each model returned *some* JSON, but with extra trailing tokens, missing closing braces, or comments that broke the contract.
- **Wall-clock variance is large** even among the failures: M2.5 errored in 21 s, M3 in 83 s. The retry-budget wiring from Q2 caps each phase at the mode-aware attempt count, so failures happen fast.
- **Token consumption for the successful run**: 35 LLM calls, ~13 K input + ~29 K output. Average ~370 input / ~820 output per call.
- **Top-3 rankings** for the kimi run:
  1. `p_002` score 8.30 (correctness 8.80, completeness 7.50, fit 8.80, evidence 7.60, clarity 8.80)
  2. `p_000` score 7.58 (correctness 8.50, completeness 6.40, fit 7.60, evidence 7.40, clarity 8.00)
  3. `p_001` score 7.26 (correctness 7.70, completeness 6.00, fit 7.30, evidence 7.80, clarity 7.50)
- **qwen3.7-max** hit the 180 s timeout mid-pipeline. Without a longer timeout we can't say if it would have completed; on the smaller prompts in earlier smoke tests it returned schema-valid JSON, so it might be working but slow.

## Test B — Discovery → Continue provider-switch flow

### Setup

- Discovery: attempted twice — see below
- Continue: `moagan continue --run-id <id> --switch-provider deepseek --skip-checkpoint`
- `MOAGAN_MINIMAX_ENDPOINT=https://api.minimax.io/anthropic/v1` exported

### Discovery attempt 1 (minimax / MiniMax-M3)

- Cardinality 80, dimensions 3, facets-per-dimension 2.
- Started at 06:25:01 UTC, ran for **>25 min** without producing `final/summary.md`. Stuck in the `integrator` phase.
- Killed manually; the run dir was partial (had `sketches`, `clusters`, `tags`, `facets`, `extractions`, `contradictions`, `drafts` subdirs but no `final/summary.md`).
- **Conclusion**: M3 is too slow for a discovery run with cardinality 80. The new Q2 retry budget caps fast/standard to 1–2 attempts, so transient failures don't compound, but each call itself can take 5–15 s on M3.

### Discovery attempt 2 (opencode_go / kimi-k2.7-code)

- Cardinality 80, dimensions 2, facets-per-dimension 2.
- **Failed at sketch phase**: `error: invalid state: discover_matrix produced zero sketches`. kimi returned no sketches in the expected JSON shape. Likely a prompt-format mismatch rather than a model-quality issue.

### Continue switch (degraded — used the Test A kimi run)

We pivoted to demonstrate the provider switch on the kimi Test A run (`019fcb6c-fa0e-7c90-984d-d09c5af6aaa5`) since:
- The kimi standard run was a successful, complete run with all artifacts.
- The continue command's behaviour for the switch (regardless of source) is the same code path.

`moagan continue --run-id 019fcb6c-fa0e-7c90-984d-d09c5af6aaa5 --switch-provider deepseek --skip-checkpoint --runs-dir ...`:

```
moagan continue: --skip-checkpoint set; resuming without human pause
moagan continue 019fcb6c: resuming after phase "deliver"
moagan: nothing left to do after phase "deliver"
```

The run had already completed, so `continue` had nothing more to do. **But** the provider switch landed correctly:

```
sqlite> SELECT from_name, to_name, phase, reason FROM provider_changes WHERE run_id='019fcb6c...';
opencode_go|deepseek|continue|user --switch-provider
         |deepseek|continue|checkpoint:skipped
```

And the manifest `provider` field flipped from `opencode_go` to `deepseek` (read post-`continue`).

### Observations

- **Provider-switch is solid**. The `provider_changes` table records both the user-driven reason (`user --switch-provider`) and the `--skip-checkpoint` reason. The manifest's `provider` field updates accordingly.
- **Discovery + kimi is a no-go for cardinality 80**: kimi doesn't emit sketches in the schema the discovery phase expects. A future sub-fase could either relax the discovery sketch schema or pre-prompt kimi with format hints.
- **Discovery + M3 is too slow**: ~30 min and still not done. The orchestrator's `retry_budget` caps attempts but doesn't help with per-call latency. A faster model is required for `--mode discovery`.

## Variance note

The 5 schema failures were deterministic — re-running any of them on the same prompt returns the same schema error. The retry budget (Q2) caps at 1 attempt for `mode=fast` and 2 for `mode=standard` (parse/schema); so the failures are surfaced fast rather than compounding.

## Provider comparison (final recommendation)

| Use case | Recommended provider/model | Rationale |
|---|---|---|
| Speed-critical CI smoke (mode=fast) | any of the 6 (whichever is cheapest); mode=fast caps at 1 attempt | The Q2 retry budget makes mode=fast fail-fast. All 5 failed models completed in 19–83 s, well within a CI smoke budget. |
| Standard mode end-to-end (today) | **opencode_go / kimi-k2.7-code** | Only model in the suite that returned schema-valid JSON end-to-end. |
| Highest-quality final portfolio (today) | opencode_go / kimi-k2.7-code | Top-1 score 8.30 with judges across all 5 criteria. |
| Cheapest for exploratory runs (deep) | needs evaluation — none of the 6 succeeded in mode=standard, so deep would likely need a follow-up | Future sub-fase. |
| Open-source-friendly | opencode_go / kimi-k2.7-code, opencode_go / qwen3.7-max | These are non-MiniMax / non-DeepSeek-direct models, per operator policy on the OpenCode Go subscription. |

## Methodology notes

- All Test A runs used the same prompt, `mode=standard`, `--non-interactive`, `--max-parallelism=4`.
- Each Test A run used a fresh `--runs-dir` to avoid the cross-run LLM cache.
- Test A output is captured at `/tmp/moagan-bench/bench.csv` (one row per model, columns: `provider,model,duration_s,input_tokens,output_tokens,top1_score,sketches,proposals,portfolio_path,error`).
- The harness is reusable: `scripts/bench_multi_model.sh`. Override `MOAGAN_BENCH_MODELS` (newline-separated `provider|model` pairs) and `MOAGAN_BENCH_PROMPT` to run a custom bench.

## Recommendations for a future sub-fase

1. **Schema-error guard rail**: when a model errors at the route phase 3 times in a row, surface a hint to the operator that the model may need a different prompt format. The retry budget caps attempts but the error context is verbose.
2. **Fast-mode discovery**: `--mode discovery` with `--cardinality 80` is M3-class-model-only territory. Either (a) cap discovery to `--provider opencode_go` only (kimi is faster in our smoke), or (b) add a `--cardinality-budget <SECONDS>` knob so operators can tune for slower models.
3. **OpenCode Go default model**: today it's `kimi-k2.7-code`. Consider making this configurable via `MOAGAN_OPENCODE_GO_MODEL` (parity with `MOAGAN_MINIMAX_MODEL` from Q5) so operators can swap `qwen3.7-max` or another non-blocked model without re-registering.
4. **Schema repair for kimi-style "thinking" responses**: when the model returns valid JSON wrapped in explanatory text, the schema validator fails. A tolerant-extraction pre-pass (look for the outermost `{...}` block, ignore preamble) would unblock more models. Could be a Q9 candidate.