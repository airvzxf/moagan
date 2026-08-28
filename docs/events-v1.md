# Stdout Events Schema (v1)

The `moagan` binary emits typed domain events on **stdout** as NDJSON
(one JSON object per line). The output is intended for machine
consumers (CI pipelines, dashboards, `jq` scripts) and is silenced when
stdout is a TTY so the operator's terminal stays clean.

## Activation

By default the emitter writes whenever stdout is not a TTY (i.e. when
`moagan … > events.jsonl` or `moagan … | jq …` is used). Operators can
override with the new global flag:

```bash
moagan … --event-format jsonl   # TTY-aware default (write when not TTY, silent on TTY)
moagan … --event-format off     # silence
MOAGAN_EVENT_FORMAT=off moagan …   # env var
```

The same `MOAGAN_EVENT_FORMAT` env var is honoured so the operator can
configure once in their shell rc.

## Decision-event verbosity (`--decision-format`)

Decision events are emitted independently of the rest of the bus and
have their own verbosity knob. The `Decision` kind is curated: only
nine `decision_kind` strings are produced, and each is classified as
either **Summary** (always emitted at the default verbosity) or
**AllOnly** (suppressed unless `--decision-format=all`). The split
deliberately keeps the default `summary` mode quiet; the high-volume
kinds (`cache_hit`, `cache_miss`, `judge_verdict`,
`category_assigned`) would otherwise produce dozens of events per run.

```bash
moagan … --decision-format summary   # default; curated set only
moagan … --decision-format all      # everything (dashboards, audits)
moagan … --decision-format off      # silence every Decision event
MOAGAN_DECISION_FORMAT=all moagan …   # env var (same precedence as flag)
```

Resolution order (highest first):

1. `MOAGAN_DECISION_FORMAT` env var (`off` / `summary` / `all`;
   unknown values fall back to `summary`).
2. Explicit `--decision-format` flag.
3. Default (`summary`).

The classification table is internal to
`src/telemetry/stdout_events.rs::classification`. New `decision_kind`
strings added by future commits MUST update the table; unknown
kinds default to Summary so every emit site is visible until classified.

### Curated `decision_kind` strings

| `decision_kind` | Emit site | Level | Payload |
|---|---|---|---|
| `winner_picked` | `src/phases/rank.rs` | Summary | `{proposal_id, score, runner_up_id?, runner_up_score?}` |
| `low_confidence_winner` | `src/phases/rank.rs` | Summary | `{top_score, threshold, gap}` |
| `cluster_skipped` | `src/phases/cluster_proposals.rs` | Summary | `{reason, size, threshold}` |
| `category_assigned` | `src/phases/discover_summary.rs` | AllOnly | `{sketch_id, category, confidence, sources}` |
| `repair_applied` | `src/phases/repair.rs` | Summary | `{proposal_id, repair_kind, attempts}` |
| `judge_verdict` | `src/phases/judge.rs` | AllOnly | `{proposal_id, score, passed, threshold}` |
| `portfolio_finalized` | `src/phases/deliver.rs` | Summary | `{proposal_id, ranking_strategy, alternatives}` |
| `cache_hit` | `src/llm/provider.rs` | AllOnly | `{cache_key, role, model}` |
| `cache_miss` | `src/llm/prompt_cache.rs` | AllOnly | `{cache_key, reason}` |

## Wire format

Each event is a single JSON object terminated by `\n` (LF). There is
**no leading array, no envelope, no shared metadata block** — every
event is self-describing. Two readers (`jq` and an event-by-event
consumer) can split on `\n` and parse each line independently.

```jsonl
{"kind":"run_start","schema":1,"ts":"2026-08-25T22:14:02.412Z","run_id":"…","mode":"fast","provider":"minimax","model":"MiniMax-M3","prompt_hash":"sha256:…"}
{"kind":"phase_start","schema":1,"ts":"2026-08-25T22:14:02.412Z","phase":"intake","seq":0}
…
{"kind":"phase_end","schema":1,"ts":"…","phase":"intake","seq":0,"elapsed_ms":2410,"status":"ok"}
…
{"kind":"llm_call","schema":1,"ts":"…","call_id":"…","phase":"intake","role":"intake","provider":"minimax","model":"MiniMax-M3","elapsed_ms":1812,"ok":true,"input_tokens":482,"output_tokens":120,"retry_count":0}
…
{"kind":"probe","schema":1,"ts":"…","probe_kind":"temperature","candidate":0.6,"iteration":3,"provider":"minimax","model":"MiniMax-M3","outcome":"accepted"}
…
{"kind":"run_end","schema":1,"ts":"…","run_id":"…","status":"ok","exit_code":0,"elapsed_ms":241823,"artefacts":{}}
```

Every event carries a top-level `schema: 1` field. The schema is
versioned independently of `moagan`'s own version. **Additive changes**
(new optional field, new event `kind`) keep `schema: 1`.
**Breaking changes** (rename, type change, removed field) bump to
`schema: 2` and are documented in a new `events-vN.md`.

## Event kinds

| `kind`         | When emitted                                   | Notable fields |
|----------------|------------------------------------------------|----------------|
| `run_start`    | After `cli::dispatch_with_run_id` returns its `DispatchResult`. The `run_id` / `mode` / `provider` / `model` / `prompt_hash` fields are stamped from the resolved command; read-only commands fall back to the `"<read-only>"` sentinel for `run_id`. Removed in v0.11.2 the legacy pre-dispatch placeholder; the move to post-dispatch was required to keep `pipeline_span.run_id` and `Event::RunStart.run_id` byte-identical across the run (the pre-v0.11.2 implementation emitted `null` for ~98.9% of in-flight events because the span fields were patched AFTER dispatch returned). | `run_id`, `mode`, `provider`, `model`, `prompt_hash` |
| `run_end`      | After the dispatcher returns (success or err). | `run_id`, `status`, `exit_code`, `elapsed_ms`, `artefacts` |
| `phase_start`  | At the start of every `Phase::execute`.         | `phase`, `seq` |
| `phase_end`    | On successful `Phase::execute` completion.     | `phase`, `seq`, `elapsed_ms`, `status: "ok"` |
| `phase_error`  | When a `Phase::execute` returns `Err`.          | `phase`, `seq`, `error`, `exit_code: 0` (placeholder) |
| `llm_call`     | On successful `provider.send` (non-probe).     | `call_id`, `phase`, `role`, `provider`, `model`, `elapsed_ms`, `ok`, `input_tokens`, `output_tokens`, `retry_count` |
| `discovery_iteration` | Per sketch loop iteration in discovery. | `n`, `total`, `cell_dim`, `cell_facet`, `temperature`, `replica`, `sketch_index`, `outcome` |
| `probe`        | Per auto-probe call (temperature / max_tokens). | `probe_kind`, `candidate`, `iteration`, `provider`, `model`, `outcome: "accepted"\|"rejected"\|"indeterminate"` |
| `warning`      | When `Telemetry::warn` is called.               | `code`, `level`, `phase?`, `details` |
| `decision`     | At curated decision points throughout the pipeline (see `--decision-format` below). Verbosity controlled by `--decision-format`. | `decision_kind`, `payload` |

> **v0.11.1**: `iteration` is now populated for `probe_kind=temperature`
> as well as `probe_kind=max_tokens`. The temperature probe tags every
> emit with the sequential index of the call within the parallel
> fan-out (0, 1, 2, …) so operators can correlate the NDJSON timeline
> with the per-batch `[t=0.0, t=0.1, t=0.2]` ordering — the
> `max_tokens` probe had already been emitting `iteration: 0` since
> v0.11.0, and the parity closes the gap that the temperature
> auto-probe emitted no `iteration` field at all.

The list grows over time. Consumers SHOULD ignore unknown `kind`s
(forwards compatibility) and unknown fields (per the JSON-LD
idiom). The `schema` field is the only REQUIRED discriminator.

## Canonical usage

### CI: collect run summary

```bash
moagan run --mode fast --provider mock:mock-model --prompt "x" \
    > events.jsonl
jq -c 'select(.kind == "run_end")' events.jsonl
# { "kind":"run_end", "schema":1, "ts":"…", "run_id":"…",
#   "status":"success", "exit_code":0, "elapsed_ms":241823,
#   "artefacts":{"final_md":"…/final/portfolio.md", …} }
```

### Stream LLM calls to a dashboard

```bash
moagan … 2>log.jsonl | jq -c 'select(.kind == "llm_call") | {call_id, role, elapsed_ms, ok}'
```

### Audit probe outcomes

```bash
moagan … 2>log.jsonl | jq -c 'select(.kind == "probe") | {probe_kind, candidate, outcome}'
```

### Error path

```bash
moagan … 2>log.jsonl | jq -c 'select(.kind == "phase_error" or .kind == "warning")'
```

## Cross-reference: stderr

Since v0.12.0 the two streams are no longer split by event type:
they are split by **level** via the `RoutingWriter` in
`src/main.rs` (around `fn init_tracing`, line 295).

- **Default** — `INFO`/`DEBUG`/`WARN` events go to **stdout**; `ERROR`
  and the panic hook go to **stderr**. The split is
  implemented in the writer (per-event `make_writer_for` decision
  on `Level::ERROR`), not in two `tracing_subscriber` layers,
  so it survives `tokio::spawn` workers cleanly.
- **`--log-to-stderr` / `MOAGAN_LOG_TO_STDERR=1`** — inverts the
  default: `INFO`/`DEBUG`/`WARN` events go to **stderr**,
  `ERROR` events go to **stdout**. The flag
  is **deprecated** as of v0.12.0 and is scheduled for removal in
  v0.14.0; new scripts should use shell redirection
  (`1> out.jsonl 2> errors.jsonl`) instead.

In both modes the **content** of stderr is the same `tracing`
event stream that goes to stdout: `file:line:column` source
attribution, `target: "moagan::..."` module path, span context
(pipeline, phase, iteration, llm_call, probe). The split is by
stream, not by audience — operators pipe whichever stream carries
the level they care about.

Operators use stderr for **errors and warnings** (the panic hook,
the `tracing::warn!` audit logs) and stdout for **everything else**
(events + INFO logs). The two are intentionally disjoint streams;
they are not redundant because each carries a different level
slice of the same event stream.

## Compatibility

- The emitter swallows write errors silently. A broken stdout
  (e.g. `head |` closing the pipe, SIGPIPE on a downstream filter)
  must not crash the run. The run's exit code is the operator's
  signal: 0 on a normal run even if stdout events failed to
  flush, non-zero if the underlying pipeline failed.
- The `schema` field is the only contract. New fields can be
  added; new `kind`s can be added; existing field types can
  only be widened (e.g. `i32 → i64`), never narrowed.
- The emitter is single-threaded via a `Mutex<Stdout>`. Concurrent
  tasks emit events serially, so partial writes never interleave
  between events. Operators who pipe through `head -n 100` may
  see SIGPIPE on the emitter side; the run continues.