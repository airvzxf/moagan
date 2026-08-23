# Auto-detected sampling temperatures

LLM upstreams do not advertise the exact temperature range they accept in a
machine-readable way. Anthropic-compat endpoints pin `temperature ∈ [0.0, 1.0]`,
OpenAI-compat endpoints typically allow `(0.0, 2.0]`, and a few relays
(DeepSeek-direct, certain OpenCode Go routes) cap the value at `1.0` and
return HTTP 400 + `temperature must be between 0 and 1` otherwise. Hard-coding
a global cap is the same brittleness the [`max_tokens` auto-probe](max-tokens-auto.md)
removes — a relay can tighten the cap without warning and the next run breaks.

`moagan` (v0.9.11+) probes each `(provider, model)` pair at first startup
to discover the discrete set of supported sampling temperatures. The result
is cached at `~/.local/share/moagan/temperatures_auto.toml` and consulted on
every subsequent call: out-of-range requests are rewritten to the nearest
accepted value via `TemperatureTable::nearest_supported(...)` and the call
proceeds with a `tracing::warn!` so the operator can see when a clamp fires.

## The algorithm

The probe tests 21 candidate temperatures per `(provider, model)`:

```
[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9,
 1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8, 1.9, 2.0]
```

Spans `0.0` (deterministic decoding) through `2.0` (the OpenAI-compat
baseline) in `0.1` increments. The canonical constant lives at
`TEMPERATURE_PROBE_VALUES` in `src/llm/temperature_probe.rs:61`.

Each candidate is tried in isolation with a tiny deterministic payload
(`"Reply with the single character: 1"`, `max_tokens = 16`, 5 s per-probe
HTTP timeout) and classified by HTTP status plus body fingerprint:

| Outcome | Meaning |
|---|---|
| `Accepted` | HTTP 2xx with a valid response body — the upstream honours the value. |
| `Rejected` | HTTP 4xx (`400`, `422`) — the upstream rejects the value as out-of-range. |
| `Indeterminate` | Timeout, 5xx, transport error, or empty body — the algorithm records nothing for that candidate and falls through. |

The algorithm fans the candidates out in groups of 3
(`TEMPERATURE_PROBE_BATCH_SIZE = 3`) so 21 candidates become exactly 7
batches, which keeps the upstream from seeing a single 21-shot burst.
`0` is a legal override that fans every candidate out in parallel; the CLI
flag `--batch-size 0` is the escape hatch for operators on a private relay
that can absorb the burst.

The probe deliberately bypasses the circuit breaker (no
`BreakeredProvider` wrapping) and the cross-run cache, so a probe that
comes back `Rejected` does not count against the runtime's breaker window
nor poison the steady-state cache.

## When it runs

There are two complementary entry points:

- **Runtime auto-probe** — `ProviderRegistry` schedules one background
  probe per fresh `(provider, model)` the first time the registry sees
  it. The probe writes through to `<MOAGAN_HOME>/temperatures_auto.toml`
  so the next startup picks the cached set up without re-running.
- **Operator-driven probe** — `moagan probe temperature --provider
  PROVIDER:MODEL [--persist-union] [--batch-size N] [--dry-run]`. The
  CLI reuses the same `detect_supported_temperatures` algorithm and
  writes through the same sidecar. See the cheatsheet §20 for the full
  flag matrix.

Both paths use the same on-disk sidecar, so an operator-pinned entry from
the CLI is visible to the runtime auto-probe on the next startup and vice
versa.

## When to disable it

Disable the auto-probe when the cost of a 21-shot HTTP sweep is
prohibitive or when the provider cannot be reached from the test runner:

- **Smoke tests against a real provider.** Every CI run would otherwise
  pay 21 sequential probes per fresh model. `scripts/smoke.sh`,
  `scripts/smoke_multimodel.sh`, and `scripts/e2e_audit_proxy.sh` all
  disable the temperature auto-probe for exactly this reason (the
  variable name mirrors the `max_tokens` auto-probe:
  `MOAGAN_TEMPERATURE_AUTO=false` — see `src/llm/temperature_probe.rs`
  for the exact env var name your build accepts; older builds may use
  the same knob via `config.toml`).
- **Sandboxed / offline runs.** The probe needs at least one successful
  round-trip; if the network is locked down the probe exits cleanly
  with the cached value (or an empty set if there is no cache).
- **Reproducible benchmarks.** When the first call must take the same
  warm path it took during the previous run, freeze the probe so the
  cache file is not rewritten.

Disable via the standard env var (`MOAGAN_TEMPERATURE_AUTO=0` or
`=false`) or by hand-editing `temperatures_auto.toml` and marking every
entry with `auto = false`.

## How the runtime uses the cached set

Every LLM dispatch goes through `RunContext::dispatch_to_provider`
(`src/phases/phase.rs:1016`). When the runtime carries a
`TemperatureTable` (it always does after the v0.9.11 wiring), the gate
runs **before** the capability resolver:

```rust
if let (Some(t), Some(table)) = (req.temperature, self.temperature_table.as_ref())
    && let Some(clamped) =
        table.nearest_supported(&self.default_provider, &self.default_model, t)
    && (clamped - t).abs() > f32::EPSILON
{
    tracing::warn!(
        provider = %self.default_provider,
        model = %self.default_model,
        role = %req.role.as_str(),
        requested = %t,
        clamped_to = %clamped,
        "temperature outside supported set; clamped at dispatch (safety net)"
    );
    req.temperature = Some(clamped);
}
```

`nearest_supported` is the absolute-distance minimiser. On ties (two
cached temperatures equally close to `requested`) the **first appearance
in `temperatures`** wins; because `TEMPERATURE_PROBE_VALUES` is sorted
ascending, the tiebreak resolves to the lower temperature on a half-step
tie and to the higher on a non-half-step tie.

The discovery pipeline has a parallel rewriter in
`src/discovery/coordinator.rs:520`: every per-provider temperature
profile in the matrix is rewritten against the auto-discovered set so
the per-cell fan-out and the cache-key cardinality reflect the
post-clamp reality (a `0.7` that gets clamped to `0.5` no longer counts
as a distinct cell from the explicit `0.5` in the same profile).

The dispatch gate is the safety net for every other path (per-role
default, profile override, legacy callers that pass `req.temperature =
Some(_)` directly); the boundary rewriter is the up-front normaliser for
the matrix-profile path. Both consult the same `TemperatureTable`.

## How to read `temperatures_auto.toml`

The cache lives at `${MOAGAN_HOME}/temperatures_auto.toml` (default
`~/.local/share/moagan/temperatures_auto.toml`). Its shape:

```toml
schema_version = 1

[providers.minimax.MiniMax-M3]
temperatures = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]
detected_at = "2026-08-21T10:00:00Z"
verified_at = "2026-08-22T11:30:00Z"
auto = true
attempts = 7

[providers.opencode_go.kimi-k3]
temperatures = [1.0]
detected_at = "2026-08-21T10:00:05Z"
verified_at = "2026-08-22T11:30:05Z"
auto = true
attempts = 7

[operator_caps.minimax]
temperatures = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]
auto = false
detected_at = "2026-08-22T12:00:00Z"

[operator_caps.opencode_go]
temperatures = [1.0]
auto = false
detected_at = "2026-08-22T12:00:01Z"
```

| Field | Meaning |
|---|---|
| `schema_version` | File format version. `1` today. Bumped if the schema changes. |
| `providers[provider][model].temperatures` | The accepted set, in canonical probe order. |
| `providers[provider][model].detected_at` | ISO-8601 timestamp of the initial successful probe. |
| `providers[provider][model].verified_at` | ISO-8601 timestamp of the most recent successful verification probe. Equal to `detected_at` on the first probe of a fresh model. |
| `providers[provider][model].auto` | Always `true` for entries the probe produced. Operators can hand-edit to `false` to freeze a known good set without removing the entry. |
| `providers[provider][model].attempts` | How many probe batches the algorithm ran (useful for telemetry: 7 for the default 21-candidate / batch-size-3 fan-out). |
| `operator_caps[provider].temperatures` | The temperatures the operator allows for this provider. Order is not significant. |
| `operator_caps[provider].auto` | Always `false` for an operator-pinned entry. |
| `operator_caps[provider].detected_at` | ISO-8601 timestamp the cap was written. |

Delete the file to force a fresh probe. Rename the file to `*.disabled`
to keep the entries on disk while skipping the probe.

## The `operator_caps` map

`operator_caps` is the operator-pinned per-provider cap that
`moagan probe temperature --persist-union` writes. The cap is the
**union** of every temperature the operator has allowed under the
same provider, so a fresh run on a new model inherits the temperatures
the operator already vetted for a sibling model on the same provider.
On a `(provider, model)` lookup the runtime intersects the
auto-discovered set with the operator's cap, so an operator who has
explicitly whitelisted `T = 0.0..1.0` cannot accidentally regress to a
`T = 1.5` acceptance that the auto-probe happens to discover on a
permissive relay.

The `--persist-union` semantics are deliberate: union (not intersection)
preserves the principle of "do not restrict what a model already
demonstrated it accepts". Intersection would silently shrink the cap
the moment one model rejects a value another model accepts.

## Tuning the batch size

`TEMPERATURE_PROBE_BATCH_SIZE = 3` matches the v0.7.1 `max_tokens`
tightening-batch size so the two auto-probes share the same fan-out
semantics; a future refactor can tune one without touching the other.

| Value | Effect |
|---|---|
| `1` | Sequential probe; slowest but safest for shared upstreams. |
| `3` (default) | 7 batches of 3 candidates in parallel; matches the runtime. |
| `7` | 3 batches of 7 candidates in parallel; faster but harder on the upstream. |
| `21` / `0` | Every candidate in parallel; one batch, one round-trip. Use only on a private relay. |

The CLI `--batch-size` flag is the only knob that overrides the runtime
default at probe time. The runtime auto-probe always uses `3`; if you
need a different batch size at startup, run `moagan probe temperature`
manually first and let the next startup read the cached value.

## Troubleshooting

- **"Probe timed out"** — the provider rejected every probe without
  returning a response, or the network is unreachable. The probe exits
  cleanly with the cached value (or an empty set if there is no
  cache). Check `~/.local/share/moagan/temperatures_auto.toml`; if the
  entry's `verified_at` is older than `detected_at` (or the entry is
  missing entirely), the cache file is stale and the probe never
  ran.
- **"Provider rejects everything"** — some providers return 4xx for
  any temperature above their hard cap. The probe treats 4xx as a
  "rejection" and excludes the candidate from the set. If the
  provider changes the limit between runs, re-detect by deleting the
  entry.
- **"Schema version mismatch"** — the on-disk file has a future
  `schema_version`. The probe refuses to load future versions and
  falls back to an empty set. Upgrade moagan first, then re-run, or
  delete the file to force a fresh discovery.
- **"Saved cache is being overwritten every run"** — the cache file
  is rewritten when the auto-probe is enabled (the default) and the
  current probe returns a different set. Disable the auto-probe via
  `MOAGAN_TEMPERATURE_AUTO=false`, or set
  `providers[provider][model].auto = false` to hand-pick a set.
- **"Operator cap is being ignored"** — the runtime always intersects
  the auto-discovered set with the operator cap; the cap cannot
  *expand* the discovered set, only narrow it. To accept a value the
  auto-probe rejected, edit `providers[provider][model].temperatures`
  directly or disable the auto-probe for that pair.
- **"Mock provider returns an empty set"** — by design. The mock
  provider skips the probe and `TemperatureTable::supported_for`
  returns an empty `Vec<f32>` for it; `nearest_supported` returns
  `None`, and the runtime leaves `req.temperature` untouched. This
  keeps per-phase temperature decisions deterministic in tests.
