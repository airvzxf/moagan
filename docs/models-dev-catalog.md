# Static models.dev catalog

`moagan` reads provider and model metadata from [models.dev][models-dev]
on first startup and caches it locally for one hour. The catalog is the
single source of truth for every flag the runtime uses to decide what
to send (and what *not* to send) to a given `(provider, model)` pair:
temperature knobs, reasoning effort, tool-call support, modality
filters, attachment support, prompt pricing, and so on. The cache
coexists with the per-`(provider, model)` `max_tokens` probe
described in [`docs/max-tokens-auto.md`](max-tokens-auto.md); the two
files serve different jobs and never overwrite each other.

[models-dev]: https://models.dev/

## What is the models.dev catalog?

models.dev is an open, community-curated index of LLM providers and the
models they expose. As of August 2026 it tracks **183 providers** and
their model rosters; the JSON payload (`https://models.dev/api.json`)
is roughly **3.6 MB**, served over HTTPS, and refreshed upstream on an
**hourly cadence**. The catalog payload is read-only from moagan's
point of view: we download it, parse it, and never write back.

The schema we depend on is small and stable:

| Field | Type | Purpose in moagan |
|---|---|---|
| `providers[id].name` | string | Display name (e.g. `MiniMax (minimax.io)`). |
| `providers[id].models[id].name` | string | Model identifier (e.g. `MiniMax-M3`). |
| `providers[id].models[id].temperature` | bool | Gate the `temperature` field in request bodies. |
| `providers[id].models[id].reasoning` | bool | Gate `reasoning_tokens` and reasoning blocks. |
| `providers[id].models[id].reasoning_options` | array of `{kind: "toggle"}` | Optional reasoning effort selection. |
| `providers[id].models[id].tool_call` | bool | Gate the `tool_choice` field. |
| `providers[id].models[id].modalities.input` | array of strings | Filter the prompt by allowed input types. |
| `providers[id].models[id].modalities.output` | array of strings | Decide whether to parse multimodal output. |
| `providers[id].models[id].attachment` | bool | Allow file/image attachments to be forwarded. |
| `providers[id].models[id].interleaved.field` | string | Tag in the response that holds reasoning content. |
| `providers[id].models[id].family` | string | Logical family grouping for cross-provider routing. |
| `providers[id].models[id].limit.context` | number | Maximum input+output window in tokens. |
| `providers[id].models[id].limit.output` | number | Maximum output tokens per call. |
| `providers[id].models[id].cost.input` | number | USD per input token. |
| `providers[id].models[id].cost.output` | number | USD per output token. |
| `providers[id].models[id].cost.cache_read` | number | USD per cached input token. |
| `providers[id].models[id].cost.cache_write` | number | USD per cache-miss input token. |

The fields above are the only fields moagan reads; any other field is
ignored. We do not validate the catalog against a frozen schema
upstream — the loader treats unknown fields as inert and reports a
warning the first time per-field per-run.

## Cache policy

The catalog is cached at:

```
${MOAGAN_HOME:-~/.local/share/moagan}/models_dev.json
```

(equivalent to `MoaganHome::models_dev_path()`). The file is a verbatim
copy of the upstream JSON, gzip-compressed on disk by `tokio::fs` after
the first successful fetch. The loader writes it atomically through
`AtomicWriter` so a partial download never replaces a good cache.

| Behaviour | Trigger |
|---|---|
| Cache hit, fresh (< TTL) | Use cache. No network. |
| Cache hit, stale (>= TTL) | Re-fetch from `https://models.dev/api.json`. |
| Cache miss (no file) | Re-fetch. First run after `lefthook install` does this. |
| Re-fetch succeeds | Rewrite cache. Update `etag` + `fetched_at_unix`. |
| Re-fetch fails + cache present | Log a warning, fall back to stale cache. Run continues. |
| Re-fetch fails + no cache + not offline | Return `Error::ModelsDevUnavailable`. Exit 40. |
| Re-fetch fails + no cache + `MOAGAN_MODELS_DEV_OFFLINE=true` | Return `Error::ModelsDevOffline`. Exit 30. |

The default TTL is **1 hour** (3600 s). Override with:

```bash
export MOAGAN_MODELS_DEV_REFRESH_HOURS=6   # 6h TTL
export MOAGAN_MODELS_DEV_REFRESH_HOURS=0   # always re-fetch (testing only)
```

Force the offline path explicitly with:

```bash
export MOAGAN_MODELS_DEV_OFFLINE=true
```

`MOAGAN_MODELS_DEV_OFFLINE=true` makes the loader skip every network
attempt. If the cache exists it is used regardless of TTL; if the
cache is missing the run aborts with `Error::ModelsDevOffline`. This is
the test-suite default so smoke tests never depend on a live
connection.

The cache is per-user, not shared. `MoaganHome::resolve()` honours
`MOAGAN_HOME` so a CI job can pin a fresh home and start with an
empty cache; a developer workstation keeps the cache warm across
sessions.

## Capability flags

moagan reads ten capability flags from the catalog and uses each one
to gate a specific runtime decision. The table below is exhaustive —
if a flag is missing from the catalog for a `(provider, model)` pair
the loader treats it as `false` (the safe default) and emits a
`warn!`-level trace.

| Flag | Type | What it gates | When `false` |
|---|---|---|---|
| `temperature` | bool | Whether to send the `temperature` field in the request body. | Omit `temperature` entirely; let the upstream pick a default. |
| `reasoning` | bool | Whether `reasoning_tokens` and reasoning blocks are allowed. | Do not send `reasoning_tokens`; do not parse reasoning content. |
| `reasoning_options` | `[{kind: "toggle"}]` | UI affordance for an optional reasoning-effort toggle. | No toggle is shown to operators. |
| `tool_call` | bool | Whether to send `tool_choice` and tool definitions. | Do not send `tool_choice`; strip tool definitions before the call. |
| `modalities.input` | array of strings | Filter prompts by allowed input types (`text`, `image`, `audio`, `video`, `pdf`). | Default `["text"]` — refuse non-text prompts. |
| `modalities.output` | array of strings | Decide whether to parse multimodal output (image/audio responses). | Default `["text"]` — treat any non-text output as a parse failure. |
| `attachment` | bool | Whether file/image attachments may be forwarded to the model. | Strip attachments before sending; return `Error::AttachmentUnsupported` if the prompt requires them. |
| `interleaved.field` | string | The tag in the response payload that carries reasoning content (e.g. `"reasoning_content"`). | Treat the response as text-only — never try to extract reasoning. |
| `family` | string | Logical family for cross-provider routing (`anthropic`, `openai`, `google`, etc.). | No family match — `provider_compatible(from, to)` returns `false`. |
| `limit.context` | number | Fallback `max_input_tokens` ceiling. | Use the hardcoded constant (8 192). |
| `limit.output` | number | Fallback `max_output_tokens` ceiling. | Use the hardcoded constant (4 096). |
| `cost.input` | number | USD per input token for budget estimation. | Estimate `cost_usd = 0`; do not include the row in cost aggregates. |
| `cost.output` | number | USD per output token for budget estimation. | Estimate `cost_usd = 0`; do not include the row in cost aggregates. |
| `cost.cache_read` | number | USD per cached input token. | Treat cache hits as `cost.input` for accounting. |
| `cost.cache_write` | number | USD per cache-miss input token. | Treat cache writes as `cost.input` for accounting. |

The list mirrors the catalog payload directly — there is no internal
normalisation layer. If a future models.dev release renames a field,
the loader will report `missing field <name>` on the first run and
the operator can open a follow-up issue.

## Merge precedence

When more than one source defines a capability for the same
`(provider, model)` pair, the precedence is:

```
Probed cap    > Catalog limit    > Config TOML     > Hardcoded constant
Catalog flag  > Config default   (per flag)
```

The "probed cap" comes from `src/llm/probe.rs` and is the live
`max_tokens` ceiling discovered by the auto-probe
([`docs/max-tokens-auto.md`](max-tokens-auto.md)). The "catalog limit"
is `limit.context` / `limit.output` from models.dev. The "Config TOML"
is `[providers.<name>]` in `~/.config/moagan/config.toml`. The
"hardcoded constant" is the default in `src/llm/capabilities.rs`
(e.g. `OPENCODE_GO_MAX_TOKENS_CAP = 16_384`).

Worked example for `(minimax, MiniMax-M3)`:

| Source | `limit.context` | `limit.output` | Used? |
|---|---:|---:|---|
| Probe (`src/llm/probe.rs`) | 524 288 | — | **yes** |
| Catalog (`limit.context` = 200 000) | 200 000 | 8 192 | yes (only when probe is absent) |
| Config TOML (`[providers.minimax]`) | — | — | no (operator never set it) |
| Hardcoded (`DEFAULT_MAX_TOKENS`) | 1 000 000 | — | never reached |

Worked example for `(opencode_go_anthropic, kimi-k2)`:

| Source | `limit.context` | `limit.output` | Used? |
|---|---:|---:|---|
| Probe | n/a (probe skipped for OpenCode Go) | n/a | **yes — but only the opencode_go hardcap** |
| Catalog | 200 000 | 8 192 | yes |
| Config TOML | 16 384 | 4 096 | no (operator never set it) |
| Hardcoded (`OPENCODE_GO_MAX_TOKENS_CAP = 16_384`) | 16 384 | 4 096 | never reached |

Per-flag precedence follows the same order: a flag from the catalog
overrides a flag defaulted in the config TOML, which overrides the
hardcoded constant. Operators who need to lock a model to a specific
behaviour can set the flag in `config.toml` and the catalog value
will be used unless the probe overrides it (probe wins for
`max_tokens` only).

## Alias map

The provider name in `~/.config/moagan/config.toml` does not always
match the provider id in the catalog. The loader translates moagan
provider names to catalog ids through a static alias map:

| moagan provider | models.dev provider id | Notes |
|---|---|---|
| `minimax` | `minimax` | Direct mapping; matches both naming conventions. |
| `opencode_go` | `opencode_go` | Multi-model aggregator; catalog has sub-entries. |
| `opencode_go_anthropic` | `opencode_go` | Routes through the Anthropic wire — catalog entry is shared. |
| `opencode_go_responses` | `opencode_go` | Routes through the Responses API — catalog entry is shared. |
| `deepseek` | `deepseek` | Direct mapping. |
| `openai_compat` | varies | Generic OpenAI-compat — operator supplies the endpoint; catalog lookup is skipped. |
| `mock` | — | Local-only; catalog lookup is skipped. |

If the alias is unknown (the provider is not in the static map), the
loader falls back to the moagan name verbatim and emits a
`trace!`-level log. If the catalog lookup still returns `None`, every
flag is treated as `false` and the run continues with the hardcoded
defaults.

## Disable

Two independent ways to opt out of the catalog:

1. **Offline mode** — `MOAGAN_MODELS_DEV_OFFLINE=true`. The loader
   skips every network attempt. Use this for hermetic smoke tests,
   air-gapped environments, and CI jobs that should not touch the
   network.

2. **Force re-fetch** — delete `${MOAGAN_HOME}/models_dev.json`. The
   next startup re-downloads the catalog. Useful after a provider
   rolls out a new model and you want to pick it up without waiting
   for the TTL.

You can also shorten the TTL for testing:

```bash
export MOAGAN_MODELS_DEV_REFRESH_HOURS=0   # re-fetch every startup
```

There is no `--no-models-dev` flag on the CLI; the env vars are the
single switch surface.

## Troubleshooting

- **Stale cache after a provider release** — the catalog lags the
  upstream release by at most one TTL window (1 h by default). Set
  `MOAGAN_MODELS_DEV_REFRESH_HOURS=0` to force a re-fetch, or delete
  the cache file. Both are reversible.

- **Partial data on first run** — if the cache file is corrupt or
  truncated, the loader logs `models_dev cache truncated; falling
  back to defaults` and continues with every flag set to `false`.
  Delete the file to force a clean re-fetch.

- **Missing provider** — if `moagan doctor --capabilities` reports
  `provider <name> not in catalog`, add an alias to the static alias
  map (see "Alias map"). Until then, the provider runs with all
  hardcoded defaults.

- **Missing model** — a `(provider, model)` pair that the provider
  exposes but the catalog does not yet list. Every flag defaults to
  `false`; the run continues. Open an upstream issue at
  <https://github.com/modelsdev/models.dev> if the model is GA.

- **Fetch failure (network down)** — the loader logs a warning at
  `warn!` level and falls back to the stale cache. If no cache
  exists, the run aborts with `Error::ModelsDevUnavailable` (exit
  40). To proceed anyway, set `MOAGAN_MODELS_DEV_OFFLINE=true` and
  the loader will use the hardcoded defaults.

- **Fetch failure (HTTP 5xx)** — retried once with a 500 ms backoff.
  On the second failure the loader treats the response as a
  transient error and falls back to the cache (or aborts with no
  cache).

- **Catalog drift** — models.dev is free to add fields at any time.
  The loader ignores unknown fields; you do not need to upgrade
  moagan to keep working. You do need to upgrade to consume new
  fields the loader does not yet know about.

## Privacy

The catalog contains **pricing, capability flags, and modality
metadata only**. There are no prompts, no user data, no telemetry,
and no PII. The cache is a verbatim copy of the upstream JSON; it
never includes the request bodies moagan sends.

The cache lives under `${MOAGAN_HOME}/`, which is per-user. Operators
who want to scrub the cache can delete
`${MOAGAN_HOME}/models_dev.json` at any time; the next run will
re-fetch. Operators who want to audit what moagan sent upstream can
enable `--log-level trace` and grep for the catalog fetch — the URL
(`https://models.dev/api.json`) is the only network call, and it
carries no credentials.

The `User-Agent` sent on the catalog fetch is the standard `reqwest`
default (no override). Operators who need a custom `User-Agent` —
rare, only useful for environments with strict egress allowlists —
can wrap the fetch through a forward proxy via the
`HTTPS_PROXY` / `HTTP_PROXY` env vars; moagan does not expose a
per-fetch `User-Agent` knob.

## References

- [models.dev][models-dev] — upstream catalog and contribution
  guidelines.
- [`docs/max-tokens-auto.md`](max-tokens-auto.md) — sibling document
  describing the runtime `max_tokens` probe that lives alongside the
  catalog cache.
- [`docs/proposal-03-add-ons.md`](proposal-03-add-ons.md) — add-on
  catalog. The models.dev integration is tracked under D.30 / D.31 /
  D.32 (v0.7.1 batch, released 2026-08-12).
- [`docs/cli-cheatsheet.md`](cli-cheatsheet.md) §0 + §6 + §12 +
  §15 — CLI surface for `moagan inspect --capabilities`,
  `moagan doctor --capabilities`, and `moagan telemetry cost`.
- [`src/fs_layout.rs`](../src/fs_layout.rs) — `MoaganHome` and the
  `models_dev_path()` helper.
- [`src/llm/capabilities.rs`](../src/llm/capabilities.rs) — the
  in-process capability matrix that the catalog augments.

[models-dev]: https://models.dev/