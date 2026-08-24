# Moagan

[![ci](https://github.com/airvzxf/moagan/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/airvzxf/moagan/actions/workflows/ci.yml)
[![release](https://github.com/airvzxf/moagan/actions/workflows/release.yml/badge.svg)](https://github.com/airvzxf/moagan/actions/workflows/release.yml)
[![codeql](https://github.com/airvzxf/moagan/actions/workflows/codeql.yml/badge.svg?branch=main)](https://github.com/airvzxf/moagan/actions/workflows/codeql.yml)
[![cargo-audit](https://github.com/airvzxf/moagan/actions/workflows/cargo-audit.yml/badge.svg?branch=main)](https://github.com/airvzxf/moagan/actions/workflows/cargo-audit.yml)
[![Dependabot](https://img.shields.io/badge/dependabot-enabled-02569b?logo=dependabot)](https://github.com/airvzxf/moagan/blob/main/.github/dependabot.yml)
[![AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](https://github.com/airvzxf/moagan/blob/main/LICENSE)

> Multi-agent system for technical problems through massive solution exploration, curation, and ranking.

Moagan is a Rust binary that orchestrates multiple LLM providers to explore, validate, critique, repair, judge, and rank competing technical proposals. It enforces hard constraints, preserves real dissent, and only lets synthesis replace its sources when the synthesis proves itself.

## Modes

| Mode | Cardinality | Purpose |
|---|---|---|
| `fast` | ~6 calls | Quick top-3 candidates. |
| `standard` | ~20 calls | Balanced proposals + critics + judges. |
| `deep` | (v0.2+) | DAG decomposition + specialised execution. |
| `explore` | (v0.2+) | Wide fan-out, no synthesis. |
| `batch` | (v0.2+) | Deterministic, non-interactive. |
| `discovery` | 40–500 sketches (v0.2+) | Knowledge base by category, no winner. |

## Quickstart

```bash
# Build
cargo build --release

# Smoke (mock provider, no API key)
./target/release/moagan run --mode fast \
    --prompt "List the seven colors of the rainbow in order" \
    --provider mock:mock-model

# Live run (requires MINIMAX_API_KEY)
export MINIMAX_API_KEY=sk-cp-...
./target/release/moagan run --mode fast \
    --prompt "List the seven colors of the rainbow in order" \
    --provider minimax:MiniMax-M3
```

At startup, Moagan best-effort loads the first `.env` found from the current working directory upward. Existing environment variables are never overwritten, so values explicitly set in the shell take precedence over `.env`. A successful load reports the file path on stderr; set `MOAGAN_QUIET=1` to suppress that notice without disabling loading. A missing `.env` is silently ignored.

### Auto-detected `max_tokens`

Moagan probes each `(provider, model)` once at first startup to
discover the real `max_tokens` ceiling. The discovered values are
persisted at `~/.local/share/moagan/max_tokens_auto.toml` and
verified on every subsequent run. Disable with
`MOAGAN_MAX_TOKEN_AUTO=0` or set `max_token_auto = None` in
`~/.config/moagan/config.toml`. See
[`docs/max-tokens-auto.md`](docs/max-tokens-auto.md) for the full
algorithm and tuning knobs.

### Auto-detected `temperatures`

Since v0.9.11, Moagan also auto-detects the supported
sampling-temperature set per `(provider, model)` and persists it at
`~/.local/share/moagan/temperatures_auto.toml`. The probe runs once
per fresh model on first startup in the background; the runtime
rewrites out-of-range requests to the nearest valid value via
`TemperatureTable::nearest_supported(...)` and emits a
`tracing::warn!` so the operator can see when a clamp fires. The
operator-driven counterpart is
`moagan probe temperature --provider PROVIDER:MODEL [--persist-union]
[--batch-size N] [--dry-run]`. See
[`docs/temperatures-auto.md`](docs/temperatures-auto.md) for the
algorithm, the sidecar format, and the `--persist-union` operator
cap.

## Architecture

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the module layout, persistence model, and provider/prompt contracts.

## License

GNU Affero General Public License v3.0 or later. See [`LICENSE`](LICENSE).

Copyright (C) 2026 Israel Alberto Roldan Vega.
