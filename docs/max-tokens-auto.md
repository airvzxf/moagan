# Auto-detected `max_tokens`

LLM providers do not advertise their real `max_tokens` ceiling in a
machine-readable way. The OAuth/backend surface lists one number, the
chat-completion surface accepts another, and the streaming-vs-non-streaming
paths often disagree. Hard-coding a `MAX_TOKENS_CAP` per provider is a
losing bet: the moment a vendor rolls a new model the constant is wrong.

`moagan` (v0.7.0+) probes each `(provider, model)` pair at first startup
to discover the actual ceiling. The discovered value is cached at
`~/.local/share/moagan/max_tokens_auto.toml` and verified with a single
lightweight probe on every subsequent startup. The probe is opt-in but
enabled by default (`Some(1024)`), so out-of-the-box behaviour is
"discover on first run, then reuse" with a safety floor of 1024 tokens.

## When to disable it

Disable the auto-probe when the cost of a sequential HTTP sweep is
prohibitive or when the provider cannot be reached from the test runner:

- **Smoke tests against a real provider.** Every CI run would otherwise
  pay ~30 sequential probes per fresh model. `scripts/smoke.sh`,
  `scripts/smoke_multimodel.sh`, and `scripts/e2e_audit_proxy.sh` all
  export `MOAGAN_MAX_TOKEN_AUTO=false` for exactly this reason.
- **Sandboxed / offline runs.** The probe needs at least one successful
  round-trip; if the network is locked down the probe will exit cleanly
  with the cached value (or the default floor if there is no cache).
- **Reproducible benchmarks.** When the first call must take the same
  warm path it took during the previous run, freeze the probe via
  `MOAGAN_MAX_TOKEN_AUTO_SAVE=false` so the cache file is not rewritten.

Disable with:

```bash
export MOAGAN_MAX_TOKEN_AUTO=false        # or =0
export MOAGAN_MAX_TOKEN_AUTO_SAVE=false   # do not overwrite the cache
```

Or in `~/.config/moagan/config.toml`:

```toml
[providers.minimax]
name = "minimax"
model = "MiniMax-M3"
max_token_auto = 0   # disable entirely (Some(0) ≡ None)
```

`Some(0)` is equivalent to `None` (both mean "off"). `Some(N>0)` enables
the probe with a floor of `N` tokens.

## How to read `max_tokens_auto.toml`

The cache lives at `${MOAGAN_HOME}/max_tokens_auto.toml` (default
`~/.local/share/moagan/max_tokens_auto.toml`). Its shape:

```toml
schema_version = 1

[providers.minimax."MiniMax-M3"]
detected_at = "2026-08-11T11:12:34Z"
verified_at = "2026-08-12T10:00:00Z"
auto = true
max_tokens = 1024

[providers.minimax."MiniMax-M2.7"]
detected_at = "2026-08-11T11:13:02Z"
verified_at = "2026-08-11T11:13:02Z"
auto = true
ceiling = 1073741824
max_tokens = 4096
```

| Field | Meaning |
|---|---|
| `schema_version` | File format version. Numeric `u32` (`1` today). Bumped if the schema changes. |
| `providers[provider][model].detected_at` | ISO-8601 timestamp of the initial successful probe. |
| `providers[provider][model].verified_at` | ISO-8601 timestamp of the most recent successful verify probe. On the first probe the algorithm initialises it to the same value as `detected_at` (via `Utc::now().to_rfc3339()` in `src/llm/probe_table.rs`), so a fresh entry is never an empty string. |
| `providers[provider][model].auto` | Always `true` while the probe is responsible for the value. Operators can hand-edit to `false` to freeze a known good value without removing the entry. |
| `providers[provider][model].attempts` | How many probe batches the algorithm ran for this entry. Diagnostic. |
| `providers[provider][model].ceiling` | The per-provider hard ceiling the algorithm started the bisect from. Diagnostic. |
| `providers[provider][model].max_tokens` | The discovered ceiling. Clamped to `[MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING]`. |

`operator_caps[provider]` is an optional operator-pinned per-provider
cap. The cap is set with `moagan probe max_tokens --persist-min
--provider PROVIDER:MODEL` (the `--persist-min` flag is the `max_tokens`
analogue of `--persist-union` for temperatures; see
[`temperatures-auto.md`](temperatures-auto.md) for the temperatures
sidecar). The runtime intersects the auto-discovered value with the
operator cap when one is set, so an operator who has pinned a lower
cap cannot accidentally regress to a value the auto-probe happens to
discover on a permissive relay.

Delete the file to force a fresh probe. There is no `*.disabled`
rename — the runtime only consults the canonical filename.

## Tuning the floor

`ProviderConfig::max_token_auto: Option<u32>` controls the floor:

| Value | Effect |
|---|---|
| `None` | Auto-probe disabled. `max_tokens` falls back to the role-specific default in `phase.rs`. |
| `Some(0)` | Same as `None` (treated as "off"). |
| `Some(1024)` | Probe enabled; the discovered value is clamped to `>= 1024`. |
| `Some(8192)` | Probe enabled; the discovered value is clamped to `>= 8192`. Use this when you know a certain role needs at least 8K tokens and you do not want to discover a smaller ceiling. |

The upper bound is fixed at `MAX_AUTOPROBE_CEILING = 1u32 << MAX_PROBE_SHIFT
= 2^30` (about 1 billion tokens). The probe's exponential phase stops at
`2^30` and the bisect phase never exceeds it.

## Troubleshooting

- **"Probe timed out"** — the provider rejected every probe without
  returning a response, or the network is unreachable. The probe exits
  cleanly with the cached value (or `MIN_AUTOPROBE_FLOOR` if no cache).
  Check `~/.local/share/moagan/max_tokens_auto.toml`; if the entry's
  `verified_at` is older than `detected_at` (or the entry is missing
  entirely), the cache file is stale and the probe never re-ran.
- **"Provider rejects everything"** — some providers return 4xx for any
  `max_tokens` larger than they support. The probe treats 4xx as a
  "ceiling" and bisects down. If the provider changes the limit between
  runs, re-detect by deleting the entry.
- **"Schema version mismatch"** — the on-disk file has a future
  `schema_version`. The probe refuses to load future versions and falls
  back to the default. Upgrade moagan first, then re-run, or delete the
  file to force a fresh discovery.
- **"Saved cache is being overwritten every run"** — the cache file is
  rewritten when `max_token_auto_save = true` (the default) and the
  current probe returns a different value. To freeze the cache
  globally, set `MOAGAN_MAX_TOKEN_AUTO_SAVE=false`. Per-entry
  `auto = false` is **just a marker** (not a freeze flag): the
  auto-probe still overwrites the entry on a fresh probe. The
  reliable ways to pin a value are: (a) hand-edit the cache file
  to your preferred `max_tokens` AND set `auto = false` so a
  reviewer can tell the entry is hand-curated, or (b) set
  `MOAGAN_MAX_TOKEN_AUTO_SAVE=false` to skip persistence entirely
  (the in-memory table still uses your hand-edited file).
- **"Mock provider returns 1_000_000"** — by design. The mock provider
  skips the probe and uses `DEFAULT_MAX_TOKENS = 1_000_000` so
  per-phase `max_tokens` decisions are deterministic in tests.
