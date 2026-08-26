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
moagan … --event-format jsonl   # always write
moagan … --event-format off     # silence
MOAGAN_EVENT_FORMAT=off moagan …   # env var
```

The same `MOAGAN_EVENT_FORMAT` env var is honoured so the operator can
configure once in their shell rc.

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
{"kind":"run_end","schema":1,"ts":"…","run_id":"…","status":"success","exit_code":0,"elapsed_ms":241823,"artefacts":{"final_md":"…/final/portfolio.md","ranking_json":"…/rankings/ranking.json"}}
```

Every event carries a top-level `schema: 1` field. The schema is
versioned independently of `moagan`'s own version. **Additive changes**
(new optional field, new event `kind`) keep `schema: 1`.
**Breaking changes** (rename, type change, removed field) bump to
`schema: 2` and are documented in a new `events-vN.md`.

## Event kinds

| `kind`         | When emitted                                   | Notable fields |
|----------------|------------------------------------------------|----------------|
| `run_start`    | Before `cli::dispatch` runs.                  | `run_id`, `mode`, `provider`, `model`, `prompt_hash` |
| `run_end`      | After the dispatcher returns (success or err). | `run_id`, `status`, `exit_code`, `elapsed_ms`, `artefacts` |
| `phase_start`  | At the start of every `Phase::execute`.         | `phase`, `seq` |
| `phase_end`    | On successful `Phase::execute` completion.     | `phase`, `seq`, `elapsed_ms`, `status: "ok"` |
| `phase_error`  | When a `Phase::execute` returns `Err`.          | `phase`, `seq`, `error`, `exit_code: 0` (placeholder) |
| `llm_call`     | On successful `provider.send` (non-probe).     | `call_id`, `phase`, `role`, `provider`, `model`, `elapsed_ms`, `ok`, `input_tokens`, `output_tokens`, `retry_count` |
| `discovery_iteration` | Per sketch loop iteration in discovery. | `n`, `total`, `cell_dim`, `cell_facet`, `temperature`, `replica`, `sketch_index`, `outcome` |
| `probe`        | Per auto-probe call (temperature / max_tokens). | `probe_kind`, `candidate`, `iteration`, `provider`, `model`, `outcome: "accepted"\|"rejected"\|"indeterminate"` |
| `warning`      | When `Telemetry::warn` is called.               | `code`, `level`, `phase?`, `details` |
| `decision`     | Reserved for explicit decision events (curated by the dispatcher). | `decision_kind`, `payload` |

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

stderr is the **logging** stream (`tracing-subscriber::fmt::layer().json()`
when redirected, coloured text when a TTY). It carries the same
information as the stdout events but with `file:line:column` source
attribution, the `target: "moagan::..."` module path, and the span
context (pipeline, phase, iteration, llm_call, probe).

Operators use stderr for **debugging** (where did the panic happen?)
and stdout for **monitoring** (what happened across the run?). The
two are intentionally disjoint streams: stderr is line-oriented
logs, stdout is typed events. They are not redundant because
each serves a different audience.

## Compatibility

- The emitter swallows write errors silently. A broken stdout
  (e.g. `head |` closing the pipe, SIGPIPE on a downstream filter)
  must not crash the run. Operators who need to know about
  emission errors can set `MOAGAN_QUIET=1` and check the run's
  exit code (still 0 on a normal run, even if stdout events
  failed to flush).
- The `schema` field is the only contract. New fields can be
  added; new `kind`s can be added; existing field types can
  only be widened (e.g. `i32 → i64`), never narrowed.
- The emitter is single-threaded via a `Mutex<Stdout>`. Concurrent
  tasks emit events serially, so partial writes never interleave
  between events. Operators who pipe through `head -n 100` may
  see SIGPIPE on the emitter side; the run continues.