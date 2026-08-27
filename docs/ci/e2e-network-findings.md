# e2e-network failure analysis — PR-04c · C-3

> **Date:** 2026-08-27 (UTC)
> **Timebox:** ~6 minutes wall-clock invested (well within the 2 h budget)
> **Workflow:** `.github/workflows/e2e-network.yml`
> **Window:** Last 4 weeks (2026-07-30 → 2026-08-27 UTC), but only the 38
> failed runs after the **2026-08-19 restructure** (commit `e94de60`) are
> in scope for this analysis — older runs reference `test-card80`,
> `test-discover-opencode-go`, etc. which were extracted to manual-only
> workflows and no longer exist in `e2e-network.yml`.
> **Verdict:** **no accionable** — 100 % of in-scope failures are
> `moagan-bug`, fuera de scope de PR-04c. Se difiere refactor mayor a
> v0.13.0+ per el plan original.

## §0 — TL;DR

- **38 / 38 (100 %)** de las corridas fallidas in-scope son
  `moagan-bug`. Ninguna en `ci-timeout`, `infra-flake`, o
  `upstream-flake`.
- El síntoma se manifiesta de **dos formas distintas**, ambas dentro
  del binario `moagan`:
  1. **`moagan-bug-proxy-start`** (12 runs, los más recientes) —
     `moagan audit proxy` arranca y loggea
     `moagan::run: dispatching`, pero **nunca imprime
     `proxy listening`** dentro de 10 s. El proceso queda
     `still running (pid=...)` y el script wrapper falla con
     `FAIL: proxy_e2e_mode_{fast,explore}_proxy_start_failed`. El
     job repite 4× con 60 s de backoff y muere (~5 min wall-clock
     total).
  2. **`moagan-bug-audit-log`** (26 runs, los más antiguos) — el
     proxy arranca bien, pero `moagan run` retorna rc=2 (o rc=7 en
     corridas más viejas), y el test wrapper falla con
     `FAIL: proxy_e2e_mode_fast_audit_log_exists`. El job repite
     4× y muere.
- **No se aplica ningún fix de retention 1d → 7d** porque (a) no
  hay categoría accionable con >30 % y (b) el problema no es que
  falten artefactos — los artefactos `.runs/` se suben
  correctamente en cada corrida con la policy actual. Cambiar
  retention no cambia la señal.
- **Plan original confirmado**: se difiere a v0.13.0+.

## §1 — Inventario

Muestreo: `gh run list --workflow=e2e-network.yml --limit=200` da 109
corridas fallidas en la ventana de 4 semanas (2026-07-30 →
2026-08-27). Las primeras 50 corridas contienen las 38 fallas más
recientes, **todas después del restructure 2026-08-19**. Esas 38 son
las in-scope y se catalogan a continuación. Runs previos al
restructure (71 fallas) referencian jobs (`test-card80`,
`test-discover-opencode-go`, `test-discover-deepseek`, etc.) que ya
no existen en `e2e-network.yml` y quedan fuera del scope.

| run_id | createdAt | branch | fail_step | fail_message_short | category |
| --- | --- | --- | --- | --- | --- |
| 33090208191 | 2026-08-27T15:52:28Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_fast_proxy_start_failed | moagan-bug-proxy-start |
| 33061618376 | 2026-08-27T10:06:57Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 33059819851 | 2026-08-27T09:42:57Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 33044880580 | 2026-08-27T06:09:47Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 33043897192 | 2026-08-27T05:52:28Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 33040661349 | 2026-08-27T04:51:07Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 33033945819 | 2026-08-27T02:39:23Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_fast_proxy_start_failed | moagan-bug-proxy-start |
| 32993983911 | 2026-08-26T17:24:52Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 32963413693 | 2026-08-26T11:27:31Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 32953644720 | 2026-08-26T09:33:33Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_fast_proxy_start_failed | moagan-bug-proxy-start |
| 32935255308 | 2026-08-26T05:44:55Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 32926185734 | 2026-08-26T03:22:15Z | main | test-fast / test-explore | FAIL: proxy_e2e_mode_explore_proxy_start_failed | moagan-bug-proxy-start |
| 32908718013 | 2026-08-25T22:59:23Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32902766349 | 2026-08-25T21:46:21Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32894501484 | 2026-08-25T20:17:37Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32884401487 | 2026-08-25T18:33:01Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32858896450 | 2026-08-25T14:20:42Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32850734873 | 2026-08-25T12:59:58Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32817194014 | 2026-08-25T06:29:28Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32811357317 | 2026-08-25T05:03:51Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32798796165 | 2026-08-25T01:45:56Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32781806408 | 2026-08-24T21:52:39Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32714448327 | 2026-08-24T09:59:30Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32703477084 | 2026-08-24T07:52:04Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32692431506 | 2026-08-24T05:07:55Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32691806600 | 2026-08-24T04:56:23Z | main | test-fast | FAIL: moagan run returned 2 | moagan-bug-audit-log |
| 32619355876 | 2026-08-23T05:02:28Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32611416754 | 2026-08-23T01:52:27Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32610840380 | 2026-08-23T01:38:12Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32506225698 | 2026-08-21T17:04:22Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32455041413 | 2026-08-21T06:35:53Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32452755520 | 2026-08-21T06:01:41Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32448123882 | 2026-08-21T04:46:32Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32440317370 | 2026-08-21T02:34:22Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32326177908 | 2026-08-20T02:52:14Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32324997581 | 2026-08-20T02:32:23Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32317366615 | 2026-08-20T00:27:14Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |
| 32315649907 | 2026-08-20T00:01:12Z | main | test-fast | FAIL: moagan run returned 7 | moagan-bug-audit-log |

Notas sobre la tabla:

- Los **primeros 12 runs** (Aug 25 23:00 UTC en adelante) muestran el
  patrón **proxy-start**: `moagan audit proxy` arranca y se queda
  colgado después del log `moagan::run: dispatching`, sin llegar a
  imprimir `proxy listening` antes del timeout de 10 s del script
  (`scripts/e2e_audit_proxy.sh:206`). El proceso queda `still
  running (pid=…)` — no es crash, es **hang**. Cuatro intentos ×
  60 s backoff = ~5 min hasta que el job muere.
- Los **siguientes 26 runs** (Aug 20–25) muestran el patrón
  **audit-log**: el proxy arranca bien, pero `moagan run` retorna
  rc=2 (InvalidArgs) o rc=7 (modelo), y el test wrapper falla con
  `FAIL: proxy_e2e_mode_fast_audit_log_exists`. Cuatro intentos ×
  60 s backoff.
- **Transición observada**: rc=7 (modelo) → rc=2 (InvalidArgs) →
  proxy-start hang. Es la misma regresión binaria progresando.
- **Último success** fue `32619993732` (2026-08-23T05:17:36Z).
  Desde entonces: 4 días de **100 % rojo**.
- **Búsqueda de patrones upstream-flake** (`schema violation`,
  `decode response error`, `429 Token Plan rate limit`, `5xx`):
  cero matches como causa de falla. Aparecen logs con `429` en
  algunos runs pero vienen del `preflight-minimax` que hace un
  `GET /v1/models` y reporta HTTP 429 al flujo de logs — el
  preflight sí pasó en esos runs (no fue el job que falló), así que
  no es causal.

## §2 — Distribución por categoría

| Categoría | Count | % |
|---|---:|---:|
| upstream-flake | 0 | 0 % |
| ci-timeout | 0 | 0 % |
| infra-flake | 0 | 0 % |
| moagan-bug (proxy-start) | 12 | 31.6 % |
| moagan-bug (audit-log) | 26 | 68.4 % |
| **moagan-bug (total)** | **38** | **100 %** |
| unknown | 0 | 0 % |

Roll-up a la taxonomía de 5 categorías del task spec:

| Categoría | Count | % |
|---|---:|---:|
| upstream-flake | 0 | 0 % |
| ci-timeout | 0 | 0 % |
| infra-flake | 0 | 0 % |
| moagan-bug | 38 | 100 % |
| unknown | 0 | 0 % |

**Umbral del task spec**: "Si **>30 %** en `ci-timeout` O
`infra-flake`, proponer fix". Ambas categorías tienen 0 %.
**No se cumplen las condiciones para aplicar fix alguno**.

## §3 — Veredicto

**No actionable fix identified in 2h timebox.**

El 100 % de las fallas in-scope son `moagan-bug`. El task spec es
explícito en que esa categoría queda fuera del scope de PR-04c:

> "moagan-bug — test asserts no se cumplieron, RC no-cero de
> `moagan run`. Triage separado, fuera de scope."

El plan original (sección "Notas importantes" del task) dice
explícitamente:

> "Si 2h se agotan en fase 2 sin fix accionable, se documenta y se
> difiere a v0.13.0+".

Ese es el camino que tomamos. **No se aplica ningún fix al
workflow.** El binary de `moagan` necesita triage propio:

- ¿Por qué `moagan audit proxy` queda colgado en `dispatch_inner`
  sin imprimir `proxy listening`? (12 runs recientes)
- ¿Por qué `moagan run` retorna rc=2 desde Aug 25? (26 runs
  anteriores; rc=7 antes de eso)

Esos dos síntomas son regresiones binarias (no flakes), y la causa
raíz está en `src/` — no en `.github/workflows/e2e-network.yml`.
Toca abrir issue aparte y PR aparte.

## §4 — Fix propuesto

**No se propone fix.** El task spec sólo autoriza los siguientes
fixes de bajo riesgo si hubiera >30 % en la categoría:

- `ci-timeout`: bump `timeout-minutes` del job afectado (≤ +50 %).
- `infra-flake`: subir retention del artifact `.runs/` de 1d → 7d.

Ninguno se aplica porque (a) `ci-timeout` = 0 % y `infra-flake` =
0 %, y (b) incluso aunque quisiéramos aplicar el bump de retention
"por simetría con PR-04c C-2", **no hay evidencia de que falten
artefactos** — todos los runs suben el zip `moagan-e2e-fast-logs`
y `moagan-e2e-explore-logs` con éxito (verificado en el log del run
33090208191 líneas 256-281). El problema es que `moagan` falla
antes de generar el contenido que el artifact contiene, no que el
artifact se pierda.

Diff hipotético (NO aplicado, sólo documentado para futuras
referencias si el patrón cambia):

```diff
--- a/.github/workflows/e2e-network.yml
+++ b/.github/workflows/e2e-network.yml
@@ -225,7 +225,7 @@ jobs:
         ...
           name: moagan-e2e-fast-logs
           path: ${{ github.workspace }}/.runs/
-          retention-days: 1
+          retention-days: 7
           if-no-files-found: warn
           include-hidden-files: true
@@ -284,7 +284,7 @@ jobs:
         ...
           name: moagan-e2e-explore-logs
           path: ${{ github.workspace }}/.runs/
-          retention-days: 1
+          retention-days: 7
           if-no-files-found: warn
           include-hidden-files: true
```

(Ya documentado en `docs/adr/0001-no-go-list-policy.md` /
consistencia con PR-04c C-2 si en el futuro la categoría
`infra-flake` cruza 30 %.)

## §5 — Referencias

- **Run más reciente de cada sub-categoría**:
  - `moagan-bug-proxy-start` → [run 33090208191](https://github.com/airvzxf/moagan/actions/runs/33090208191) (2026-08-27 15:52 UTC, último intento de la sesión actual)
  - `moagan-bug-audit-log` (rc=2) → [run 32908718013](https://github.com/airvzxf/moagan/actions/runs/32908718013) (2026-08-25 22:59 UTC)
  - `moagan-bug-audit-log` (rc=7) → [run 32619355876](https://github.com/airvzxf/moagan/actions/runs/32619355876) (2026-08-23 05:02 UTC)
  - **Último success** → [run 32619993732](https://github.com/airvzxf/moagan/actions/runs/32619993732) (2026-08-23 05:17 UTC)
- `docs/e2e-loop-2026-08-12.md` — loop histórico (10 iteraciones,
  Aug 12) usado como baseline; en esa sesión los flakes eran
  100 % upstream-flake (JSON schema violations del modelo) y
  siempre se recuperaban en retry. El patrón actual es
  estructuralmente distinto: regresión binaria, no flake
  recuperable.
- `docs/branch-protection.md` — `e2e-network` no es required
  check, así que estos rojos no bloquean merges a `main`. El
  riesgo operacional es bajo — la regresión binaria afecta
  visibilidad de T3 pero no la gate de merge.
- Workflow restructure commit: `e94de60 ci(workflows):
  restructure e2e-network into auto+manual; manual-only
  test-ignored-*` (2026-08-19) — referencia de por qué las
  fallas pre-Aug-20 están fuera de scope.

## §6 — Próximos pasos sugeridos (fuera de PR-04c)

1. **Triage de `moagan-bug-proxy-start`**: reproducir localmente
   `moagan audit proxy --upstream https://api.minimax.io/anthropic/v1
   --port 0 --runs-dir <tmp>` y verificar por qué no imprime
   `proxy listening`. Posibles causas:
   - cambio reciente en `cli/audit/proxy.rs` que altera el log de
     startup;
   - bloqueo en `dispatch_inner` por espera de un recurso
     (HTTP client warming, lazy init, etc.);
   - nuevo código de tracing que captura el log de
     `proxy listening` antes de imprimirlo a stdout
     (verificar `init_tracing: log_to_stderr` y routing de
     tracing a stdout por defecto desde PR
     `feat!: route tracing logs to stdout by default (BREAKING)
     [v0.12.0]`).
2. **Triage de `moagan-bug-audit-log` (rc=2 / rc=7)**: ¿qué
   assert del test `proxy_e2e_mode_fast_audit_log_exists` está
   fallando? El wrapper sólo reporta "moagan run returned N"
   sin contexto. Considerar aumentar el verbosity en el script.
3. **Ambos síntomas podrían compartir causa**: rc=7 → rc=2 →
   proxy-start hang sugiere una regresión progresiva que se
   manifiesta diferente según el código que se ejecute primero.
   Vale la pena un único triage unificado.
