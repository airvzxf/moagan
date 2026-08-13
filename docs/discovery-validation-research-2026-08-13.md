# Discovery validation research — 2026-08-13

> Pendiente del operador: "los 4 sub-directorios de discovery nunca corrieron exitosamente con Token Plan / opencode.ai".

## §1. Sub-fases de discovery y los "4 sub-directorios"

`moagan discover` ejecuta 10 fases en este orden (`tests/integration_discovery.rs:186-198`):

```
intake → clarify → discover_matrix → discover_tag → discover_cluster
       → discover_contradict → discover_facet → discover_extract
       → discover_integrate → discover_summary
```

Hay **7 fases `discover_*`** que producen los sub-directorios de la spec V4 §6.5–§6.10 (`src/fs_layout.rs:325-352`):

| Fase | Role LLM | Sub-directorio |
|---|---|---|
| `discover_matrix.rs` | `Sketch` | `sketches/` (compartido) |
| `discover_tag.rs` | `Tagger` | `tags/` |
| `discover_cluster.rs` | `Tagger` | `clusters/` |
| `discover_contradict.rs` | `Tagger` | `contradictions/` |
| `discover_facet.rs` | `FacetDeriver` | `facets/` |
| `discover_extract.rs` | `Extractor` | `extractions/` |
| `discover_integrate.rs` | `Integrator` | `drafts/` + `final/cat_NN.md` |

Los **4 sub-directorios** que el operador se refiere son los 4 mapeados uno-a-uno a un **role LLM distinto**: `tags/`, `facets/`, `extractions/`, `drafts/`. Los otros 2 (`clusters/`, `contradictions/`) reusan `Role::Tagger` y heredan cobertura de `discover_tag`. Lectura: 4 pares únicos (sub-directorio, role, provider) sin cobertura e2e real.

## §2. ¿Qué es "Token Plan"?

`PlanConfig` (`src/config/mod.rs:1023-1038`): bloque declarativo `[providers.X].plan = { plan_id, limit_tokens, window_days }` en el TOML. Es **aditivo** (sin bloque → `plan: None`). Lo consume `moagan telemetry plan [<provider>] [--window-days N]` (`docs/cli-cheatsheet.md` §15.10, `src/cli/telemetry_cmd.rs:1124-1221`), que ejecuta `Db::aggregate_window_usage` (`src/storage/sqlite.rs:1576`) sobre la tabla `calls` y pinta `used / limit (pct%)` cuando hay `limit_tokens > 0`.

**No es un provider upstream ni un SKU; es una cuota rolling-window local que `moagan` usa para reportar consumo.** Validar con Token Plan = correr discovery real + `moagan telemetry plan --provider <X>` y cuadrar contadores por role contra `limit_tokens`.

## §3. opencode.ai como provider (`opencode_go`)

Un único kind `opencode_go` con dispatcher en `src/llm/opencode_go.rs:115-159` que rutea a **3 wire formats distintos** desde la base `https://opencode.ai/zen/go/v1` según el modelo del roster 2026-08-04 (`endpoint_path_for`, líneas 86-98):

- `…/v1/chat/completions` (OpenAI-compat): glm-5.1/5.2, kimi-k3, kimi-k2.7-code/k2.6, deepseek-v4-pro/flash, mimo-v2.5/pro, hy3.
- `…/v1/messages` (Anthropic-compat): `minimax-m3/m2.7/m2.5` **bloqueados por policy** (preferir directo `minimax`), qwen3.8/3.7/3.6-max/plus.
- `…/v1/responses` (OpenAI Responses): gpt-5.6-luna.

API key vía `OPENCODE_GO_API_KEY` (env o `api_keys.toml`). Hard-cap compartido `OPENCODE_GO_MAX_TOKENS_CAP = 16_384`. Override de temperatura por modelo (`kimi-k3 → 1.0` obligatorio).

## §4. Tests de discovery existentes

**Provider real:** ninguno contra `opencode_go`. El único test e2e discovery con provider real está en `scripts/e2e_audit_proxy.sh` (sub-bloque `card80`) y usa `--provider minimax` con sidecar proxy.

**Provider mock:** `tests/integration_discovery.rs` (~30 tests con `MockProvider`), `tests/integration_pr17_coordinator_wire.rs`, `scripts/smoke_discovery.sh` (120 tests grep-based, todos con `--provider mock`).

**`opencode_go` sólo aparece en `tests/integration_q3_dotenv.rs`** (verifica carga de key desde `.env` — no toca discovery).

## §5. Diagnóstico: qué falta

1. **No existe e2e discovery contra `opencode_go`** — sólo mock y minimax.
2. **No hay `[providers.opencode_go.plan]` declarado por defecto** — `telemetry plan` imprimiría `(no plan)` aunque la validación pase.
3. **No hay `OPENCODE_GO_API_KEY` operativa** en CI (sólo valor fake en el test de dotenv).
4. **Cobertura por role:** los 4 sub-directorios cubren los 4 `Role` LLM únicos en discovery (Tagger / FacetDeriver / Extractor / Integrator); los 3 que reusan Tagger heredan.

## §6. Recomendación

**No es accionable automáticamente.** Requiere:

1. **Operador:** `OPENCODE_GO_API_KEY` real + modelo no-bloqueado (ej. `kimi-k2.7-code` o `gpt-5.6-luna`; **no** la familia `minimax-*`).
2. **Decisión de producto:** declarar `[providers.opencode_go.plan]` con `limit_tokens` razonable, o documentar Token Plan como opcional para este provider.
3. **Trabajo de código (M-sized, 1 PR):** añadir sub-bloque `discover_opencode_go_<model>` en `scripts/e2e_audit_proxy.sh` paralelo al card80 `minimax`, con asserts por sub-directorio (`tags/`, `facets/`, `extractions/`, `drafts/`) + check post-run de `moagan telemetry plan --provider opencode_go`. Estimado: ~80 LoC + target `e2e-network-discover-oc` en `Makefile`.

El gap no es de código existente (los proveedores ya están wirados y testeados con wiremock); es de **cobertura e2e contra `opencode_go` con key operativa**. Hasta que el operador desbloquee el acceso upstream, el sub-agent no puede cerrarlo automáticamente.

## §7. Cierre del pendiente

**Estado**: `BLOCKED — necesita operador`. Marcar como documented-and-deferred en el audit report. El siguiente operador con acceso a `OPENCODE_GO_API_KEY` puede:

1. Crear la API key upstream.
2. Configurar `[providers.opencode_go]` en `~/.config/moagan/config.toml` con un `plan = { plan_id = "weekly", limit_tokens = 1_000_000, window_days = 7 }`.
3. Correr `scripts/e2e_audit_proxy.sh` (sub-bloque nuevo a añadir) y validar los 4 sub-directorios contra `moagan telemetry plan`.

**Costo del cierre**: 1 PR de ~80 LoC en `scripts/` + `Makefile`, gates T3 verdes en CI.

_Last updated: 2026-08-13 10:30 UTC_
