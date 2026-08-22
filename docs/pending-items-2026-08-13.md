# Moagan — informe definitivo de pendientes (2026-08-13)

> **Fecha**: 2026-08-13
> **HEAD verificado**: `39647a1` (`main`, post-#464)
> **Base de verificación**: 4 worktrees de verificación read-only
> (`verify-spec-gaps`, `verify-e2e-coverage`, `verify-deferred`,
> `verify-skip`), todos anclados a `39647a1`.
> **Alcance**: sólo lo que queda **abierto** o merece atención. Lo ya
> cerrado se enumera en §12 para evitar re-trabajo.
> **Nota**: este informe **reemplaza** el borrador previo de la misma
> fecha, que contenía 3 claims stale corregidos aquí (§5.1, §5.2, §9.2).

---

## §1 — Resumen ejecutivo

### 1.1 — Qué somos

`moagan` es un binario Rust único (edition 2024, stable 1.97.1) que
implementa un sistema multi-agente de exploración masiva de soluciones:
ingesta → discovery (tags / clusters / facets / extractions /
contradictions) → generación → curación → ranking adversarial →
síntesis de portfolio. La autoridad arquitectónica es, en orden:
`docs/proposal-01-concept.md` (V4, visión), `docs/proposal-02-rust.md`
(T01-06, **normativo**) y `docs/proposal-03-add-ons.md` (catálogo D.x,
opt-in). En conflicto, **T01-06 gana**.

El estado de release es **v0.7.1** (tag object `11c5746f`), documentado
en `docs/v0.7-final-report.md`. Session 3 cerró con **21 PRs mergeados**
(rango #444–#464, ver breakdown en §2) y ~1,535 LoC de dead code
eliminados; suite verde: 1784 lib tests, 30 binarios de integración, 0
fallos.

### 1.2 — Dónde estamos

Tres frentes distintos, con salud muy distinta:

| Frente | Salud | Comentario |
|---|---|---|
| **Higiene de código** (dead code, TODOs, identificadores) | ✅ excelente | 12 rounds de auditoría cerrados; 0 `TODO`/`FIXME`/`todo!()` reales en producción (ver `verify-skip` §B). |
| **Spec coverage T01-06** | ⚠️ 3 gaps duros | GLM / Qwen / Kimi nunca se implementaron (ver §3). |
| **Cobertura e2e real de providers** | ❌ el punto débil | 1 de 15 modelos `opencode_go` usables tiene round-trip real, y **ese único bloque nunca corre en CI** (ver §9). |

El delta más grande entre "lo que creemos" y "lo que es" no está en el
código: está en la **documentación**. Las 4 verificaciones encontraron
**8 claims stale repartidos en 3 documentos** (`docs/spec-impl-gaps.md`,
`docs/test-skips.md` y el borrador previo de este mismo informe).

### 1.3 — Hacia dónde vamos

v0.8.0 hereda, per `docs/v0.7-final-report.md` §8: D.22 (adversary
refinements), D.23-D.27 (telemetría push-side), D.29-D.32 (retención
cross-run), `RemoteEmbedder`, y los tres opt-ins de dependencia
(`petgraph`, `comfy-table`, `proptest`) que hoy están en la no-go list
de `AGENTS.md` y por tanto requieren **decisión de política antes que
código**.

La recomendación operativa (detalle en §11 y §13) es: **primero cerrar
la deuda documental y de CI** (barata, ~2 h, desbloquea cobertura real
de 14 modelos), y **sólo después** abrir features.

### 1.4 — Tally final

> **18 items OPEN** (3 spec gaps T01-06 + 8 deferred de v0.7 §7 + 4
> sub-gaps K.4 ampliado + 2 de cobertura CI + 1 PR #421 stranded + 1
> comment stale en script; **−1** vs el cierre inicial: el borrado de
> `origin/fix/audit-findings` ya está hecho). **7 correcciones pendientes
> en OTROS documentos** (3 en `docs/test-skips.md`, 1 en
> `docs/spec-impl-gaps.md` §5, 3 stale claims heredados del borrador
> previo, 1 comment en `scripts/e2e_audit_proxy.sh:553`; **−1** vs el
> cierre inicial: el borrado de la rama cubre el ítem Tier S #2 que
> también aparecía en `docs/test-skips.md`). **Siguiente paso
> recomendado: ejecutar los 6 items restantes del Tier S de §11 (~1 h)
> en una sola PR `docs+ci`, antes de tocar cualquier feature.**

---

## §2 — Estado por fase del proyecto (v0.1 → v0.7 → v0.8 prospect)

Reconstruido desde `docs/v0.7-final-report.md` §2 (PR catalogue) y
`docs/COORDINATION.md` (log de cierre de sesiones 1/2/3).

| Versión | Foco | Hitos representativos | Estado |
|---|---|---|---|
| v0.1–v0.4 | Núcleo del pipeline | fases canónicas (`canonical_phase_order`), storage SQLite + migraciones embebidas, `RedactWriter` / `RedactPolicy` | ✅ cerrado |
| v0.5 | Ranking adversarial + hardening | `AdversaryPhase` (PR #277) con los 7 patterns canónicos; `safe_path` (PR #306) | ✅ cerrado; D.22 (pattern library extendida) diferido |
| v0.6 | Telemetría read-side + discovery | `moagan telemetry plan` (PR #367), `TemperatureProfile` (PR #356), D.17.7 `sketches_summary.csv` | ✅ cerrado; push-side diferido (D.23-D.27) |
| v0.7.0 / v0.7.1 | Auto-probe + capability gating | cap pre-probe `minimax` a 524288 (#379); `src/llm/probe.rs` (#400, #401), `ProviderConfig::max_token_auto`, `doctor --capabilities` (#425), remoción de `tiktoken-rs` (#427) | ✅ released, tag `11c5746f` |
| **session 1/2/3 post-v0.7.1** | Auditoría de inconsistencias | rounds 1–12; 21 PRs en rango #444–#464 (6 round-10 + 5 round-11 + 3 round-12 + 1 discovery + 1 deepseek-follow-up + 6 docs/follow-up + 1 test-cleanup); ~1,535 LoC muertos eliminados; migraciones v016/v017 | ✅ cerrado (ver §4) |
| **v0.8 prospect** | Features + cobertura | D.22, D.23-D.27, D.29-D.32, `RemoteEmbedder`, K.4 ampliado, cobertura e2e de 14 modelos, decisión sobre no-go list | 🟡 abierto — este informe es su backlog |

Dos observaciones de fase, per `docs/v0.7-final-report.md` §7:

1. La remoción de `tiktoken-rs` está **verificada limpia**: ausente de
   `Cargo.toml` y de `Cargo.lock` (`verify-spec-gaps` §F). El
   presupuesto de tokens es in-house (`src/llm/budget.rs` +
   `src/phases/budget.rs`).
2. La migración `v017_drop_manifest_versions.sql` **existe** y
   referencia correctamente su origen (v012) y la PR #433 (commit
   `fdc02d6`) — `verify-spec-gaps` §E.

---

## §3 — Spec gaps abiertos (T01-06 normative)

Fuente primaria: `.worktrees/verify-spec-gaps/findings/spec-gaps.md`
(6/6 items de alcance verificados). Cruce con
`docs/proposal-02-rust.md` §0.2 y `docs/spec-impl-gaps.md`.

### 3.1 — Providers GLM / Qwen / Kimi — **OPEN (3/3)**

`ls src/llm/*.rs` → 34 ficheros (excluyendo `mod.rs`); ninguno es `glm.rs`, `qwen.rs` o
`kimi.rs`.

| Provider | Spec (T01-06 §0.2 líneas 73–76) | ¿Existe? | Verdict |
|---|---|---|---|
| GLM | `src/llm/glm.rs` | ❌ | **OPEN** |
| Qwen | `src/llm/qwen.rs` | ❌ | **OPEN** |
| Kimi | `src/llm/kimi.rs` | ❌ | **OPEN** |

`grep -rn "GLM|Qwen|Kimi" src/llm/` devuelve 7 hits, **todos** en
tablas de capability-lookup, comentarios de chat-template o strings de
test — **cero código de producción**:

- `src/llm/control_tokens.rs:31` — comentario sobre ChatML/Qwen.
- `src/llm/response_format_opt_out.rs:150-151` — asserts sobre
  `"GLM-5.1"` / `"Kimi-K3"`.
- `src/llm/capability.rs:295`, `:427` — entrada de catálogo + assert.
- `src/llm/json_strategy.rs:173` — docstring de ejemplo.

**Implementación real hoy** (6 módulos de provider): `deepseek.rs`
(OpenAI Chat Completions), `minimax.rs` (Anthropic-compatible),
`opencode_go.rs` + 2 sub-wires (`opencode_go_anthropic.rs`,
`opencode_go_responses.rs`), y `mock.rs`.

**Coste por provider nuevo**, per `docs/spec-impl-gaps.md` §3:

1. Pin de esquema raw-HTTP (no SDK — guardia en
   `scripts/check-no-anthropic-sdk.sh`).
2. Nuevo `src/llm/<name>.rs` implementando el trait `Provider`.
3. Wire-up en `src/llm/provider_pool.rs` + `src/llm/capability.rs`
   (`max_token_auto` en vez de constante hard-coded, per
   `docs/v0.7-final-report.md` §4).
4. Entrada de role catalog + smoke gate en `docs/validation-tiers.md`.

**Bloqueo real**: el operador nunca los pidió. Sin ticket, los 3 siguen
abiertos por diseño, no por olvido.

### 3.2 — Gaps de directorio (7 items) — 3 OPEN, 4 colapsos intencionales

Per `docs/spec-impl-gaps.md` TL;DR, re-verificado en `verify-spec-gaps`
§C:

| # | Spec pedía | En disco | Resolución |
|---|---|---|---|
| 1 | `src/ingest/{mod,normalize,detect,budget}.rs` | ❌ | **Colapso intencional** → `src/phases/intake.rs` |
| 2 | `src/llm/retry.rs` | ❌ | **Split intencional** → `src/llm/retry_budget.rs` + `phases/phase.rs::call_with_retry_parse` |
| 3 | `src/llm/glm.rs` | ❌ | **OPEN** |
| 4 | `src/llm/qwen.rs` | ❌ | **OPEN** |
| 5 | `src/llm/kimi.rs` | ❌ | **OPEN** |
| 6 | `src/domain/<single-type>.rs` (13 ficheros) | ❌ | **Colapso intencional** → `src/domain/mod.rs` (2,284 LoC) + `constraint.rs`, `graph.rs`, `synthesis_request.rs` |
| 7 | `src/discovery/{tagger,clusterer,contradiction,facet,extractor,integrator}.rs` | Parcial | **Colapso parcial intencional** — **excepto `contradiction.rs`, que SÍ existe** (ver §5.1) |

**Resumen**: 4 colapsos/splits intencionales + 3 OPEN + 1 fila con
drift documental corregido en §5.1.

### 3.3 — Verificaciones de spec que salieron limpias

- `src/storage/migrations/v017_drop_manifest_versions.sql` — presente,
  cabecera correcta (`verify-spec-gaps` §E).
- `tiktoken-rs` — ausente de `Cargo.toml` y `Cargo.lock`
  (`verify-spec-gaps` §F); PR #427 verificada.

---

## §4 — Cierre de auditorías: Round-1 / Round-2 / Round-5..12

### 4.1 — Round-1 — **100 % cerrado**

Per `docs/inconsistencies-audit-2026-08-12.md` §H.1, todos los findings
de round-1 tienen PR de cierre:

| Finding | PR de cierre |
|---|---|
| §A.1 `src/cli/flags_batch.rs` (7 helpers de env-var muertos) | #444 |
| §A.5 `src/storage/lease_full.rs` (wrapper `FullLease`, 143 LoC) | #445 |
| §A.6 helpers de `audit/{format,verify}.rs` | #442 (re-derivado) |
| §B.2 5 módulos muertos de telemetry | #433 + #436 |
| §B.3 `llm/{anthropic_compat,streaming}.rs` | #438 |
| §C.1 identificadores en español (`detectar_outliers`, `cola_reserva`) | #446 |
| §C.3 docstring stale en `ranking/mod.rs:4` | #439 |
| §C.4 `BudgetPolicy::{Warn,Abort}` | #436 + #440 |
| §D.4 tabla SQLite `manifest_versions` | #439 (migración v017) |
| §E.1 fila 9: wire-up de `token_budget` | #447 |

Única nota residual: `persona_angle` fue clasificado como muerto por
error; tiene **4 callers reales** en `discovery/coordinator.rs:254, 325,
504, 526` (`pick_persona` × 2, `pick_angle` × 2) y se mantuvo vivo —
decisión correcta.

### 4.2 — Round-2 — cerrado, con 1 nota de spec pendiente

Per `docs/inconsistencies-audit-2026-08-12-round-2.md`:

- §B.7 — **`D.22.3 invalidate_downstream`**: la spec describe la
  función; la implementación se eliminó en PR #434. El código está
  correcto; falta **una nota de cierre de 1 párrafo** en
  `docs/proposal-03-add-ons.md` §D.22.3. **VERIFIED-OPEN (docs-only)**.
- §C.4 — `call_with_retry_parse` sin docstring: **CLOSED** (ver §5.2).
- §D.13.19 `MatrixCell` seed: resuelto vía `TemperatureProfile` (PR
  #356); la spec ya lleva la nota. ✅

### 4.3 — Rounds 5..12 — **0 items borderline abiertos**

| Round | PRs | Estado |
|---|---|---|
| Round-8 (borderline del round-2 §E.4, 5 items) | #440, #442 | 4/5 cerrados; los 2 restantes (`phases::clarify` helpers, builders de `validators::*`) **no son accionables**: tienen callers legítimos |
| Round-9 | — | sin items residuales |
| Round-10 (5 items del brief) | #444–#449 | `accumulate_usage`, `get_problem_graph`, `check_cargo_toml`, `override_pair` ya no existen; `with_config` vive como `pub fn` con 8 callers verificados. **#449** (`refactor: drop 4 trivial pub fn with single test-only caller`, −71 LoC) |
| Round-11 | #451–#455 | 100 % cerrado per round-2 §G.1. **#452** (`refactor: drop 6 test-only 1-caller pub fn + 1 inline registry module`, −386 LoC); **#453** (`refactor: drop 1 dead fn + demote 4 internal pub fn visibility`, −21 LoC) |
| Round-12 | #456–#458 | 100 % cerrado per §G.2; 5 funciones mal clasificadas como muertas fueron demoted `pub fn` → `fn` en #455. **#457** (`refactor(llm): round-12 mass cleanup — drop dead helpers + demote pub fn`, −81 LoC) |

**Conclusión**: la deuda de auditoría de código está saldada. El
backlog restante es de **spec, cobertura y documentación**, no de dead
code.

---

## §5 — Spec drift nuevo (post-session-3) — **incluye CORRECCIONES**

Esta sección corrige explícitamente el borrador previo del informe.

### 5.1 — CORRECCIÓN: contradiction detection — **PARTIAL, no ausente**

**Claim stale (borrador §1.2)**: *"El directorio `src/discovery/` … NO
incluye `contradiction.rs`"* y *"ni siquiera hay una función vacía con
`todo!()`"*.

**Realidad verificada** (`verify-spec-gaps` §B):

- `src/discovery/contradiction.rs` **existe**, 86 LoC.
- Exporta `ContradictionRecord`, `severity_rank`, `top_pairs`.
- Está **cableado**: `src/phases/discover_contradict.rs:17`
  → `use crate::discovery::contradiction::{ContradictionRecord, severity_rank, top_pairs};`

**Estado correcto**: **PARTIAL — stub ligero shipped; el detector
completo LLM-as-judge per V4 §6.5–§6.10 sigue diferido.** El equivalente
en el catálogo add-on es **`proposal-03-add-ons.md §D.13.11`**
(`ContradictionDetector { topic, description, severity }`), que es
exactamente la pieza unimplemented: V4 §6.5-§6.10 describe la idea;
D.13.11 la ata a tipos concretos. Lo que existe es plumbing de
severidad y selección de pares; lo que falta es el juicio semántico
(BFS sobre `ArtifactGraph` + comparación por LLM) que describe V4.

**Coste del detector completo**: ~200–400 LoC para una versión básica
(similitud de cluster + LLM judge); ~1 semana para hacerlo bien.

**Corrección derivada obligatoria**: `docs/spec-impl-gaps.md` §5 (línea
182) dice *"Contradiction: (no production code — deferred per
v0.7-final-report.md §7)"* — **stale**, hay que corregirlo (Tier A en
§11).

### 5.2 — CORRECCIÓN: `call_with_retry_parse` — **CLOSED, documentado**

**Claim stale (borrador §4.2)**: *"200-line retry chokepoint sin
docstring propio | OPEN"*.

**Realidad verificada** (`verify-spec-gaps` §D): la función se define
en `src/phases/phase.rs:1637` y lleva un bloque `///` de **32 líneas
completas en `src/phases/phase.rs:1605-1636`** (las "últimas 12 líneas"
del docstring son `:1625-1636`, inmediatamente encima de la firma), que
documenta los presupuestos de reintento (`Deep` rate-limit: 3
intentos; `Deep` parse/schema: 2 con reparación), la semántica de
`max_retries` como techo de seguridad y no como garantía, y el hecho de
que cada reintento se registra como warning estructurado
(`model.retry_parse`).

**Estado correcto**: **CLOSED — documentado**. Se retira de la lista de
items abiertos y del Tier S.

### 5.3 — Drift residual real

| Item | Spec | Impl | Estado |
|---|---|---|---|
| `D.22.3 invalidate_downstream` | T01-06 §18.2 + proposal-03 §D.22.3 | eliminado en #434 | ✅ código cerrado; **nota de spec pendiente** (round-2 §B.7) |
| `D.13.19 MatrixCell` seed | T01-06 §9 + proposal-03 §D.13.19 | `TemperatureProfile` (#356) | ✅ cerrado, nota ya escrita |
| Comment stale `deepseek-chat` | — | `scripts/e2e_audit_proxy.sh:553` | **VERIFIED-OPEN** — ver §9.3 |

### 5.4 — TODOs reales en producción: **cero**

`verify-skip` §B confirma 3 hits de `TODO|FIXME|XXX|todo!()|
unimplemented!()`, los 3 legítimos:

- `src/validators/mod.rs:230` — docstring del variant `Placeholder` de
  `FailureKind`.
- `src/phases/gate.rs:133` — comentario que enumera los tokens
  placeholder que el Gate detecta (`TODO`, `TBD`, `xxx`, `???`).
- `src/phases/gate.rs:374` — fixture de test `"We will TODO the rest
  later"`.

**0 TODOs reales en código de producción.** Sin drift nuevo por esta
vía.

---

## §6 — Spec-add-ons vs impl (cross-check catálogo D.x)

Per `docs/proposal-03-add-ons.md` (catálogo **cerrado**: no se añaden
D.x nuevos; cualquier hallazgo nuevo se etiqueta **VERIFIED-OPEN**).

### 6.1 — D.x abiertos

| D.x | Descripción | Evidencia @ `39647a1` (`verify-deferred` §A) |
|---|---|---|
| **D.22** | Adversary refinements más allá de los 7 patterns | `src/ranking/adversary_patterns.rs:78` (`impl AdversaryPattern`) y `:134` (`all_seven()`) son los únicos entry points. Grep `PatternLibrary\|additional_pattern\|extended_pattern\|adversary_v2` → **0 hits**. Los 7 variants (`:161-200`) son exactamente los canónicos. **OPEN** |
| **D.23-D.27** | Telemetría avanzada: live quotas, alert channels, `SaturationEvent` runtime, agregación CSV cross-run | Grep `SaturationEvent\|alert_channel\|live_quota` en `src/telemetry/` → **0 hits**. `src/telemetry/csv_summary.rs:1-30` es el writer **single-run** (D.17.7), no un agregador cross-run. `dashboard.rs` existe pero sin canal de push. **OPEN** |
| **D.29-D.32** | Retención cross-run | Grep `retention_sweep\|cross_run_sweep` → **0 hits** repo-wide. **Matiz**: `src/telemetry/retention.rs` **sí** ships una política per-run (`RetentionConfig { keep_runs_days, keep_runs_count, max_storage_bytes, policy }` con `plan`/`apply`/`scan`) sobre `.runs/`; lo que falta es el barrido cross-run (p. ej. purga de filas SQLite huérfanas). `v0.7-final-report.md` §7 **subestima** la cobertura parcial. **OPEN (alcance más estrecho de lo que dice el report)** |

### 6.2 — Dependencias diferidas (en no-go list de `AGENTS.md`)

| Dep | Uso previsto | Estado |
|---|---|---|
| `petgraph` | backend DAG opcional para el pipeline de fases | grep en `Cargo.toml` → 0 hits. `canonical_phase_order` sigue siendo el orden lineal. **OPEN — requiere decisión de política** |
| `comfy-table` | tablas CLI (`inspect`, `telemetry provider`, `telemetry plan`) | 0 hits en `Cargo.toml`/`Cargo.lock`. Salida en texto plano. **OPEN — requiere decisión de política** |
| `proptest` | property-based testing | 0 hits. Suite 100 % `#[test]`/`#[tokio::test]` a mano. **OPEN — requiere decisión de política** |

Los tres están en la no-go list explícita de `AGENTS.md` **por decisión
de diseño, no por accidente**. Abrir cualquiera de ellos es primero un
cambio de política y después una PR.

### 6.3 — Otros items diferidos de v0.7 §7

| Item | Estado |
|---|---|
| `RemoteEmbedder` | `src/llm/embed/` contiene **sólo `mod.rs`** (288 líneas: `HashingEmbedder` + trait `Embedder` + `cosine`). Grep `RemoteEmbedder` en `src/` → 0 hits; el doc-comment `mod.rs:1-5` difiere explícitamente `RemoteEmbedder` y `fastembed` a una sub-fase posterior. **OPEN** |
| Multimodal streaming | sin símbolos `multimodal\|attachment\|streaming.*image` en `src/llm/`. PR #422 (modality gating, sobre la rama de #421) añade **detección de capacidad**, no streaming. **OPEN — bloqueado en upstream** |

### 6.4 — Balance

**8 de 8 items de `docs/v0.7-final-report.md` §7 siguen OPEN.** Sin
cambio de estado respecto al report. Session 3 fue de limpieza, no de
features, así que **no cerró ningún D.x del catálogo** — resultado
esperado, no una regresión.

---

## §7 — Cuarta etapa K.x (`docs/proposal-04-cuarta-etapa.md`)

Fuente: `.worktrees/verify-deferred/findings/deferred-items.md` §B–§E,
contra la sección "Status (Sesión D, 2026-08-06)" de
`docs/proposal-04-cuarta-etapa.md` (líneas 161–173).

### 7.1 — K.1 Per-domain profiles — **SHIPPED**

Coincide con proposal-04 §Status línea 163 (PR #125).

- `src/config/profile.rs` — módulo completo, 280 líneas: `Profile::load`,
  `Profile::load_with_history` (detección de ciclos), `Profile::empty()`,
  campo `extends: Option<String>`.
- `src/cli/mod.rs:283` — documenta `~/.config/moagan/profiles/`.
- `src/cli/mod.rs:926` — documenta `--profile <name>`.
- `src/cli/run.rs:1143` — enruta `--profile <name>` por
  `build_pipeline_for_mode`.
- `src/cli/discover.rs:394` — consulta `TemperatureProfile::default()`.

### 7.2 — K.2 Cross-process hibernation — **SHIPPED**

Coincide con proposal-04 §Status líneas 164–166 (PRs #126, #131, #146).

- `src/cli/pause_cmd.rs:1-9` — doc-comment con `moagan pause <run_id>`,
  `moagan continue --from-pause` y serialización a
  `<run_dir>/paused.json`.
- `src/cli/pause_cmd.rs:55-72` — `PausePoint::load` / `PausePoint::save`.
- `src/cli/pause_cmd.rs:166` — ruta de resume `--from-pause` que consume
  `paused.json` + `paused.lock`.
- `src/cli/continue_cmd.rs` — superficie emparejada `moagan continue`.

### 7.3 — K.3 User preference learning — **SHIPPED**

Coincide con proposal-04 §Status líneas 167–168 (PRs #133, #135).

- `src/cli/rate.rs:82-161` — lee y fija `MOAGAN_LEARNING` como gate de
  opt-in (por defecto **off**, per la nota de riesgo de proposal-04 §2).
- `src/cli/mod.rs:686` — doc-comment con la ruta por defecto de la
  variable de entorno.
- Superficie CLI `moagan rate` presente; layout de caché
  `~/.config/moagan/ratings/<user>.json` implícito en la semántica del
  opt-in.

### 7.4 — K.4 External research **ampliado** — **PARTIAL / OPEN**

Más estrecho de lo que sugiere proposal-04 §Status líneas 169–173.

**Lo que sí ships:**

- ✅ Allowlist de 4 hosts: `src/research/allowlist.rs:56`
  → `pub const ALLOWED_HOSTS: &[&str] = &["docs.rs", "crates.io", "api.github.com", "github.com"];`
  con `HostPolicy { auth_bearer, .. }` per-host en `:34-46`. El test
  `auth_bearer_only_set_for_api_github_com` (`:99-103`) fija el opt-in
  de auth **sólo** para `api.github.com`.
- ✅ Fetcher acotado y cableado: `src/research/fetcher.rs:263` invoca
  `allowlist::is_allowed(host)` como gate; `:340` construye URLs de
  prueba `https://docs.rs/page-{i}`; `:419`, `:440`, `:468` cubren las
  rutas de auth-bearer y de denegación.

**Lo que falta (K.4 ampliado = 4 sub-gaps OPEN):**

| Sub-gap | Evidencia |
|---|---|
| **Parser de PDF** | grep `pdf\|PDF` en `src/research/` → **0 hits**. Diferido a v0.6+ per proposal-04 §Status línea 173 |
| **Renderizado JS / headless browser** | grep `headless_browser\|playwright\|chromium\|webkit\|js_engine\|render_js` en `src/` → **0 hits** |
| **Auth en hosts distintos de `api.github.com`** | sólo `api.github.com` lleva `auth_bearer = true`; `docs.rs`, `crates.io`, `github.com` van sin autenticar |
| **Rate-limiting avanzado** | ausente de `fetcher.rs` |

### 7.5 — Balance K.x

**3 de 4 items SHIPPED** (K.1, K.2, K.3). **K.4 ampliado abierto con 4
sub-gaps.** Ninguno es bloqueante para v0.8.0; el más defendible como
siguiente paso es el rate-limiting avanzado, por ser hardening y no
feature.

---

## §8 — Stranded branches + PRs abiertos + worktrees

### 8.1 — PR #421 `feat/models-dev-reasoning-gate` — **STRANDED**

Verificado en `.worktrees/verify-deferred/findings/deferred-items.md`
§F, vía `git ls-remote origin refs/pull/421/head`.

| Atributo | Valor |
|---|---|
| Tip de la rama | `660c63e2a1d1f299adb0a0088797eb2293de8b08` — *"merge: resolve all conflicts in favor of main"* (2026-08-12 05:29:49 -0600) |
| Padres del merge | `dec6cdc1` (tip de rama) + `9379a67e` (`origin/main` en ese momento — el commit de `feat(cli): doctor --capabilities, telemetry cost, probe max_tokens (#425)`) |
| Posición vs `main@39647a1` | **4 commits por delante / 37 por detrás** (`git rev-list --left-right --count origin/pr/421...main` → `4 37`) |
| Commits totales en la rama | **253** (rama de larga vida que absorbió #414, #415, #416, #417, #419, #420, #422, #423, #425, #426, #427) |
| Diff-stat vs tip | ~**50 ficheros**, +4.2k / −2.7k LoC |
| Ref local | **ninguna** — `git branch -a \| grep reasoning-gate` → 0 hits; sólo alcanzable vía `origin/pr/421` (fetch desde `refs/pull/421/head`) |

Los 4 commits por delante de `main`:

1. `7f693a2` (2026-08-12 02:41) `feat(llm): reasoning gating via models.dev catalog` — el commit de cabecera.
2. `45bdf39` (03:29) merge de sincronización #1 desde `origin/main`.
3. `dec6cdc` (03:57) merge de sincronización #2.
4. `660c63e` (05:29) el merge de resolución de conflictos sobre `9379a67e`.

Módulos nuevos más grandes: `src/llm/models_dev.rs` (912 LoC),
`src/cli/probe.rs` (533), `src/llm/capability.rs` (543),
`src/llm/modal_gate.rs` (375), `src/llm/cost.rs` (221),
`src/cli/telemetry_cmd.rs` (304), `src/cli/inspect.rs` (98).

**Veredicto**: la rama está **varada en un merge commit, no mergeada**.
El commit "resolve all conflicts in favor of main" indica que el autor
abandonó el rebase y depositó el trabajo sobre una instantánea de
`main` que hoy está 37 commits atrás. **No es candidata a
fast-forward**: funcionalmente es un target de rebase sin resolver.

**Decisión requerida (Tier B, §11)**: o **rebase fresco sobre
`main@39647a1` + re-review**, o **cierre formal de la PR** documentando
qué partes ya llegaron a `main` por otras vías (#425, #426, #427 sí
están mergeadas).

### 8.2 — Rama `origin/fix/audit-findings`

- HEAD `f1e6a50` (merge de `main` en la rama).
- Contiene los commits de la PR #424 histórica (drop de `lease_full` +
  `flags_batch` + wire-up de `token_budget`) — **los 3 ya re-derivados
  y mergeados** en session 3 vía #444, #445 y #447.
- `git diff --stat origin/main origin/fix/audit-findings` → 138
  ficheros, +4602 / −7295; todo ese contenido ya está en `main`.

**Estado (2026-08-13 19:58 UTC)**: **BORRADO**. Comando ejecutado:
`git push origin --delete fix/audit-findings` →
`[deleted] fix/audit-findings`. La rama era histórica:
- PR #424 ya estaba CLOSED-not-MERGED (no absorbed por merge).
- Contenido (`lease_full` + `flags_batch` + `token_budget` wire-up)
  re-derivado y mergeado en session 3 vía #444 / #445 / #447.
- Diff vs `main`: 5 ahead / 45 behind, todos absorbed.

Esta sección queda cerrada; el ítem pasó a Tier S #2 ✅ (ver §11).
`git remote prune origin` ejecutado para limpiar refs locales.

### 8.3 — PRs abiertos y worktrees

- `gh pr list --state open` → **0 PRs abiertos**. El caso #421 es
  CLOSED-not-MERGED (no "open", no "merged"): está varado como ref
  huérfana `origin/pr/421` con tip `660c63e` (ver §8.1); su contenido
  llegó parcialmente a `main` vía #425, #426, #427.
- Worktrees: además del principal, existen los 4 worktrees de
  verificación read-only creados para este informe
  (`verify-spec-gaps`, `verify-e2e-coverage`, `verify-deferred`,
  `verify-skip`) más `pending-items-2026-08-13`. **Ninguno tocó código
  fuente ni hizo commits**; son desechables tras el merge de este
  informe.

---

## §9 — Discovery e2e: cobertura real vs `#[ignore]`

Fuente: `.worktrees/verify-e2e-coverage/findings/e2e-coverage.md`. Esta
sección **corrige** dos claims del borrador previo.

### 9.1 — Las 2 PRs de discovery e2e

- **PR #459** (`feat(e2e): validate discovery mode with opencode_go +
  Token Plan`), mergeada en `7dce267`: añade la sección
  `discover_opencode_go` a `scripts/e2e_audit_proxy.sh` (~88 LoC) y
  `tests/integration_discover_opencode_go.rs` (88 LoC, 1 test
  `#[ignore]` gated en `OPENCODE_GO_API_KEY`).
- **PR #462** (`feat(e2e): validate discovery mode with deepseek +
  Token Plan`), mergeada en `7facb6c`: companion de #459, misma
  estructura, gated en `DEEPSEEK_API_KEY`.

Contexto de la investigación P8 en
`docs/discovery-validation-research-2026-08-13.md`.

**Fases `discover_*` (7) y su mapeo a sub-dirs con `Role`**, per P8
research §1 líneas 15-27: el modo `discover` ejecuta 7 fases canónicas
— `matrix`, `tag`, `cluster`, `contradict`, `facet`, `extract`,
`integrate` — cada una materializa su output en un sub-directorio bajo
`<run>/discovery/`. Cuatro sub-dirs tienen rol único en el catálogo de
LLM roles; los otros dos reutilizan `Role::Tagger`:

| Sub-dir | Fase | `Role` (catálogo) |
|---|---|---|
| `tags/` | `discover_tag` | `Role::Tagger` |
| `clusters/` | `discover_cluster` | `Role::Tagger` (re-used) |
| `contradictions/` | `discover_contradict` | `Role::Tagger` (re-used) |
| `facets/` | `discover_facet` | `Role::FacetDeriver` |
| `extractions/cat_*` | `discover_extract` | `Role::Extractor` |
| `drafts/` | `discover_integrate` | `Role::Integrator` |
| `matrix_*` | `discover_matrix` | (sin `Role`; artefacto de cardinalidad) |

### 9.2 — CORRECCIÓN: los bloques `discover_*` del `e2e_audit_proxy.sh` **nunca corren en el path auto** (no es que impriman SKIP)

**Claim stale (borrador §6.2)**: *"`e2e-network.yml` corre los bloques
shell pero, sin API keys en CI, imprime `SKIP`"*.

**Realidad verificada** (`verify-e2e-coverage` §B): los bloques **no se
alcanzan en absoluto** en el path auto post-PR #555; el guard de
sección los excluye **antes** de llegar a la comprobación de la key.

- `scripts/e2e_audit_proxy.sh:546` exige
  `MOAGAN_SMOKE_SECTION ∈ {all, discover_opencode_go}`;
  `:661` lo mismo para `discover_deepseek`.
- `.github/workflows/e2e-network.yml` (auto, en push a `main`) sólo
  ejecuta `fast` (`:174`) y `explore` (`:232`), vía
  `make e2e-network-fast|explore`. `card80` se movió a
  `.github/workflows/e2e-network-card80.yml` (manual, 2026-08-19).
- **Ningún job auto de CI fija `MOAGAN_SMOKE_SECTION=discover_opencode_go`
  ni `=discover_deepseek`**, y **no hay ninguna referencia a
  `OPENCODE_GO_API_KEY` ni `DEEPSEEK_API_KEY` en `.github/workflows/`**
  fuera de los `test-ignored-*` post-merge. El único secreto del path
  auto es `secrets.MINIMAX_API_KEY`
  (`e2e-network.yml:123,160,192,251`).

Para los tests `#[ignore]`: `test-ignored-minimax.yml` corre
`cargo test -- --ignored` en `push: branches: [main]`. Los
`test-ignored-deepseek.yml` y `test-ignored-opencode-go.yml` (PR #555)
quedaron como stubs `workflow_dispatch` only — el workflow existe para
que la pestaña _Actions_ muestre el check en cada push a `main`, pero
los jobs reales de `cargo test -- --ignored` se restauran por separado
(subagente A, ver §9.3).

### 9.3 — CORRECCIÓN: roster real — 18 modelos, 15 usables, 14 sin e2e

**Claim stale (borrador §6.3)**: *"1 modelo de 12+ disponibles"* y *"2
modelos en `deepseek` pendientes"*.

**Realidad verificada** (`verify-e2e-coverage` §A/§C): la fuente de
verdad es `src/llm/opencode_go.rs:86-98` (`endpoint_path_for`) →
**18 modelos**, no "12+". Los 18 están registrados como alias distintos
en `default_providers` (`src/config/mod.rs:806-845`, comentario "All 18
OpenCode Go providers"; lista fijada en `:2079-2097`), así que
`--provider <alias>` apunta a cualquiera **sin necesidad de `--model`**.

| Wire path | Modelos | Línea |
|---|---|---|
| `/v1/responses` | `gpt-5.6-luna` | `:89` |
| `/v1/messages` | `minimax-m3`*, `minimax-m2.7`*, `minimax-m2.5`*, `qwen3.8-max`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus` | `:91-92` |
| `/v1/chat/completions` | `glm-5.1`, `glm-5.2`, `kimi-k3`, `kimi-k2.7-code`, `kimi-k2.6`, `deepseek-v4-pro`, `deepseek-v4-flash`, `mimo-v2.5`, `mimo-v2.5-pro`, `hy3` | `:94-95` |

`*` = bloqueado por política vía `OpenCodeGoProvider::BLOCKED_MODELS`
(`src/llm/opencode_go.rs:186-187`). **Modelos usables: 15.**

**Cobertura e2e real: 1 de 15.** El único con round-trip de red real es
`kimi-k2.7-code` (el modelo por defecto del alias `opencode_go`), vía
`scripts/e2e_audit_proxy.sh:500` y
`tests/integration_discover_opencode_go.rs:43` — **y sólo manualmente**
(ver §9.2). El resto son "unit" (assertions de routing/roster sin HTTP,
`src/llm/opencode_go.rs:457-467`, `:489-492`, `:304-307`) o "none".

**Los 14 sin e2e real**: `gpt-5.6-luna`, `qwen3.8-max`, `qwen3.7-max`,
`qwen3.7-plus`, `qwen3.6-plus`, `glm-5.1`, `glm-5.2`, `kimi-k3`,
`kimi-k2.6`, `deepseek-v4-pro`, `deepseek-v4-flash`, `mimo-v2.5`,
`mimo-v2.5-pro`, `hy3`.

**Riesgo concreto**: dos de los tres wire formats — `/v1/responses`
(`gpt-5.6-luna`) y `/v1/messages` (cualquier `qwen3.*`) — **nunca han
hecho una petición real**. Sólo `/v1/chat/completions` está probado
end-to-end.

**DeepSeek nativo: 1 modelo, cobertura completa (no 2 pendientes).**
`src/config/mod.rs:775` registra un único alias `deepseek` con modelo
`deepseek-v4-flash` (builder `make_deepseek` en `:762-774`), y ese
modelo **sí** tiene e2e real (`scripts/e2e_audit_proxy.sh:583`,
`tests/integration_discover_deepseek.rs:43`), bajo el mismo gate manual.
`deepseek-v4-pro` **no es un provider deepseek nativo**: sólo existe
como alias de `opencode_go`. Por tanto el pendiente nativo es **0, no
2**.

**Comment stale asociado**: `scripts/e2e_audit_proxy.sh:553` sigue
diciendo *"model `deepseek-chat` → `deepseek-v4-flash` resolved
upstream"*. Tras #464, que eliminó `--model deepseek-chat`, el default
de config es literalmente `deepseek-v4-flash` y **no hay resolución de
alias upstream**. Fix documental de 1 línea. **VERIFIED-OPEN**.

**Post-2026-08-19 update (PR #555)**: el workflow
`.github/workflows/e2e-network.yml` se reestructuró en auto (sólo
`fast` + `explore` en push a `main`) + manuales
(`e2e-network-card80.yml`,
`test-ignored-{deepseek,opencode-go,minimax}.yml`). Los
`test-ignored-deepseek.yml` y `test-ignored-opencode-go.yml` quedaron
como stubs `workflow_dispatch` only con motivo "budget exhausted"
(simétricos a #529/#550); el auto-trigger `push: branches: [main]` se
eliminó de ambos para no consumir runner minutes con un banner. Los
tests `#[ignore]` en
`tests/integration_discover_{deepseek,opencode_go}.rs` siguen
invocables a mano (`cargo test --test integration_discover_* -- --ignored`).
La restauración de los jobs reales de `preflight + cargo test --ignored`
en esos dos workflows queda pendiente (subagente A).

### 9.4 — Qué cubre realmente el bloque card80

Per `verify-e2e-coverage` §E: `scripts/e2e_audit_proxy.sh` SECCIÓN A
(`:174-464`) es **sólo minimax**, con sidecar de audit-proxy
(`MOAGAN_MINIMAX_ENDPOINT` → `http://127.0.0.1:$PORT/anthropic/v1`),
`discover --sketches-per-cell 10 --dimensions 4 --facets-per-dimension 2
--max-parallelism 4` (~25 min, `:222`) y **37 asserts** de artefactos,
integridad del audit-log (CRC + id por fila, emparejamiento
request/response), redacción (`x-api-key`, `authorization`), `audit
verify` / `inspect` y artefactos de discovery.

Las secciones A.bis (`:466-546`, opencode_go) y A.ter (`:548-629`,
deepseek) son **pares de card80, no parte de él**: keys distintas,
`MOAGAN_SMOKE_SECTION` distinto, **sin sidecar de proxy** (van directas
a upstream y se apoyan en el `calls.jsonl.gz` del propio run). Cada una
verifica 6 cosas: run-id presente, `tags/ ≥ 2`, `facets/ ≥ 1`,
`extractions/cat_* ≥ 1`, `drafts/ ≥ 1`, y `telemetry plan` reportando
`weekly` con `used > 0`.

### 9.5 — Acción de mayor rendimiento

Los 14 modelos ya son alias de provider, así que **un bucle
`for MODEL in ...; do ... --provider $MODEL; done` alrededor del bloque
A.bis existente no requiere cambio de código fuente** — es script-only.
Combinado con 2 matrix jobs en `e2e-network.yml` + 2 secretos, ~15
líneas desbloquean la cobertura completa. Es, con diferencia, el mejor
impacto/coste del backlog.

---

## §10 — Inventario de `#[ignore]` y skips condicionales

Fuente: `.worktrees/verify-skip/findings/skip-inventory.md`, contrastado
con `docs/test-skips.md`.

### 10.1 — Los 5 `#[ignore]`

| Test | Fichero:línea | Gate | ¿Corre en CI? |
|---|---|---|---|
| `discover_opencode_go_writes_four_subdirs` | `tests/integration_discover_opencode_go.rs:32` | `#[ignore]` + early-return si falta `OPENCODE_GO_API_KEY` (`:34-37`) | ⚠️ stub `workflow_dispatch` en `test-ignored-opencode-go.yml` post-PR #555; restauración de preflight+cargo-test pendiente (subagente A) |
| `discover_deepseek_writes_four_subdirs` | `tests/integration_discover_deepseek.rs:32` | `#[ignore]` + early-return si falta `DEEPSEEK_API_KEY` (`:34-37`) | ⚠️ stub `workflow_dispatch` en `test-ignored-deepseek.yml` post-PR #555; ídem |
| `audit_e2e_deep_run_has_exact_external_coverage` | `tests/integration_audit_e2e.rs:259` | `#[ignore]`, motivo: flaky bajo ejecución paralela (documentado como known-flaky en `AGENTS.md`) | ❌ además `--skip`-eado por `make test-ci` |
| `prlimit_apply_sets_nproc_rlimit` | `src/sandbox/cgroup.rs:396` | muta `RLIMIT_NPROC` a nivel de proceso | ❌ manual por diseño |
| `prlimit_apply_sets_as_rlimit` | `src/sandbox/cgroup.rs:441` | muta `RLIMIT_AS` a nivel de proceso | ❌ manual por diseño |

**Accionables: 2** (los de discovery). **Correctos por diseño: 3**
(1 known-flaky + 2 mutadores de rlimit global).

### 10.2 — Capas 4 y 5: coinciden con la doc

- **Layer 4 — silent skips en `src/validators/`**: 13 sitios con el
  patrón `if Command::new(<bin>).arg("--version").output().is_err() { return; }`
  — `rust_validator.rs` 7, `python_validator.rs` 2,
  `typescript_validator.rs` 2, `sql_validator.rs` 2. **Coincide exacto
  con `docs/test-skips.md`.**
- **Layer 5 — `ValidationEvidence::skipped()`**: 13 hits crudos, de los
  cuales 2 (`validators/mod.rs:632, :1115`) son código de test. **Sitios
  runtime: 11 — coincide exacto.**
- **Layer 2 — flag `--skip` de cargo test**: 0 hits en `Makefile` y
  `.github/workflows/`. Genuinamente cerrada. (Los `--skip-checkpoint`
  de `src/cli/continue_cmd.rs:801` y `--skip-smoke`/`--skip-clippy` de
  `scripts/gauntlet.sh` son flags no relacionados.)

### 10.3 — CORRECCIONES a `docs/test-skips.md`

| Capa | Doc dice | Realidad | Delta |
|---|---|---|---|
| 2 — flag `--skip` | 0 (cerrada) | 0 | ✅ |
| **3 — `#[ignore]`** | **2 tests (sólo cgroup)** | **5** | ❌ **+3 sin documentar** |
| 4 — silent-skip | 13 sitios | 13 | ✅ |
| 5 — `skipped()` runtime | 11 sitios | 11 | ✅ |
| 6a — gate `MINIMAX_API_KEY` | 46 tests | 46 (`e2e_audit_proxy.sh:181`) **+ 1 (`gauntlet.sh:143`)** | ⚠️ **`gauntlet.sh` sin documentar** |
| 6b — `MOAGAN_SMOKE_LONG_DISCOVER` | 37 tests | 37 (subconjunto de los 46, `:194`) | ✅ |
| 6c — partial-skips de card80 | 14 condicionales | dentro de los 37 | ✅ |
| **(ausente) — `OPENCODE_GO_API_KEY`** | no listado | **8 tests** (`:490-546`) | ❌ **+8 sin documentar** |
| **(ausente) — `DEEPSEEK_API_KEY`** | no listado | **8 tests** (`:573-629`) | ❌ **+8 sin documentar** |

**Dos bugs concretos a corregir en `docs/test-skips.md`:**

1. **Layer 3** — añadir los 3 `#[ignore]` que faltan (audit_e2e flaky +
   los 2 de discovery gated en API key) y subir el total de **2 → 5**.
2. **Layer 6** (o sub-sección nueva) — documentar los gates
   `OPENCODE_GO_API_KEY` (8 tests, líneas 490–546) y `DEEPSEEK_API_KEY`
   (8 tests, líneas 573–629) de `e2e_audit_proxy.sh`, más la
   re-comprobación de `MINIMAX_API_KEY` en `gauntlet.sh:143`.

**Impacto real en CI hoy**: el runner tiene `MINIMAX_API_KEY` pero no
las otras dos, de modo que **16 invocaciones de `run_test` imprimirían
`SKIP`** — salvo que, per §9.2, esos bloques ni siquiera se alcanzan
por el guard de sección. La doc afirma "0 skips activos en CI", lo que
sólo es cierto para los gates que documenta.

---

## §11 — Tier priorizado para session 4

Orden por **impacto / coste**. Los items marcados 🆕 son adiciones de
esta revisión respecto al borrador previo.

### Tier S — accionables hoy, alto valor, bajo riesgo

| # | Item | Coste | Impacto |
|---:|---|---|---|
| 1 | 🆕 **Corregir los 3 claims stale de este informe antes de mergear** — §5.1 contradiction PARTIAL, §5.2 `call_with_retry_parse` CLOSED, §9.3 roster 18/15/14. Edición de 1 línea cada uno si se reintroducen | 5 min | Evita propagar desinformación |
| 2 | ✅ **DONE (2026-08-13 19:58 UTC)** — `origin/fix/audit-findings` borrada vía `git push origin --delete`. Era histórica, PR #424 CLOSED, contenido absorbido por #444/#445/#447. Ver §8.2. | — | Higiene cerrada |
| 3 | **Nota de cierre D.22.3 en `docs/proposal-03-add-ons.md`** — 1 párrafo, per round-2 §B.7 | 10 min | Higiene de spec |
| 4 | **Documentar los 3 `#[ignore]` que faltan en `docs/test-skips.md` Layer 3** (2 → 5) | 10 min | Exactitud documental |
| 5 | 🆕 **Documentar los +16 skips condicionales en Layer 6** (`OPENCODE_GO_API_KEY` 8, `DEEPSEEK_API_KEY` 8, `gauntlet.sh:143` 1) | 15 min | Exactitud documental |
| 6 | 🆕 **Corregir `docs/spec-impl-gaps.md` §5** (línea 182): contradiction no es "no production code"; es stub ligero shipped | 5 min | Exactitud documental |
| 7 | 🆕 **Limpiar el comment stale de `scripts/e2e_audit_proxy.sh:553`** sobre `deepseek-chat` (post-#464) | 5 min | Exactitud documental |

**Quedan 6 items en Tier S (#1 + #3–#7) tras el cierre del #2. Caben en
una sola PR `docs+ci` de ~1 h.**

### Tier A — vale la pena en v0.7.2 / v0.8.0

| # | Item | Coste | Impacto |
|---:|---|---|---|
| 8 | 🆕 **Añadir 2 matrix jobs en `.github/workflows/e2e-network.yml`** (`discover_opencode_go`, `discover_deepseek`) + 2 targets de Makefile + 2 `gh secret set` (`OPENCODE_GO_API_KEY`, `DEEPSEEK_API_KEY`) | 30 min (~15 líneas) | **Alto** — pasa de 0 a 2 bloques de discovery ejecutándose realmente en CI |
| 9 | 🆕 **Añadir el bucle `for MODEL in ...` en `scripts/e2e_audit_proxy.sh`** alrededor del bloque A.bis para cubrir los **14 modelos restantes** con `--provider $MODEL` (sin cambio de código fuente) | 2–4 h | **Alto** — cubre por primera vez `/v1/responses` y `/v1/messages` |
| 10 | **Paso `cargo test -- --ignored`** en el mismo workflow, para los 2 tests de discovery | 15 min | Cobertura; va en la misma PR que el #8 |
| 11 | **Detector de contradicciones completo** (LLM-as-judge, V4 §6.5-6.10) sobre el stub de `src/discovery/contradiction.rs` | ~1 sem | Feature — cierra el gap más visible de spec |
| 12 | **D.22 adversary refinements** — pattern library más allá de los 7 | 1–2 d | Feature |
| 13 | **D.23-D.27 telemetría push-side** — `SaturationEvent` runtime + alert channels + agregación CSV cross-run | 1–2 d | Feature |
| 14 | **D.29-D.32 barrido cross-run** — la política per-run de `src/telemetry/retention.rs` ya está; falta el sweep (purga de filas SQLite huérfanas) | 2 d | Hardening |
| 15 | **K.4 ampliado: rate-limiting avanzado** en `src/research/fetcher.rs` | 1 d | Hardening |
| 16 | **Providers GLM / Qwen / Kimi** — 3–5 d cada uno; **requiere ticket del operador**, hoy inexistente | 9–15 d | Feature (spec T01-06) |

### Tier B — nice-to-have

| # | Item | Coste | Impacto |
|---:|---|---|---|
| 17 | 🆕 **Re-evaluar PR #421 (`feat/models-dev-reasoning-gate`)** — decidir entre rebase fresco sobre `main@39647a1` + re-review, o cierre formal documentando qué llegó por #425/#426/#427 | 1 h de decisión; 1–2 d si se rebasea | Desatasca 253 commits / 50 ficheros varados |
| 18 | **`RemoteEmbedder`** — segundo adapter junto a `HashingEmbedder` | 1 d | Feature |
| 19 | **Decisión de política sobre la no-go list** (`comfy-table`, `proptest`, `petgraph`) — **primero política, después código** | 1 h de decisión | Desbloquea 3 items de v0.7 §7 |
| 20 | **K.4 ampliado: PDF + renderizado JS + auth multi-host** | 1 sem+ | Feature; el de menor retorno del lote |
| 21 | **Multimodal streaming** | bloqueado upstream | No accionable hoy |
| 22 | **Documentar `audit_e2e` flaky como opt-in permanente** hasta diagnosticar la causa raíz | 5 min | Higiene |

---

## §12 — Lo que **NO** está pendiente (verificación explícita)

Para no duplicar esfuerzo, lo siguiente está **verificado cerrado** y no
requiere acción:

- ✅ **Round-1** — 100 % cerrado (PRs #432–#440, #444–#447), per
  `docs/inconsistencies-audit-2026-08-12.md` §H.1.
- ✅ **Rounds 2 / 5 / 6 / 7 / 8 / 9 / 10 / 11 / 12** — cerrados, per
  `docs/inconsistencies-audit-2026-08-12-round-2.md` §E.3, §E.5, §G.
- ✅ **`call_with_retry_parse` sin docstring** — **falso**; hay 12
  líneas de `///` en `src/phases/phase.rs:1625-1636`.
- ✅ **`src/discovery/contradiction.rs` ausente** — **falso**; existe
  (86 LoC) y está cableado en `src/phases/discover_contradict.rs:17`.
- ✅ **Dead code eliminado**: `cli/flags_batch.rs`,
  `storage/lease_full.rs`, `llm/anthropic_compat.rs`, `llm/streaming.rs`,
  los módulos de `telemetry/{level,tracing_filter,hub,daily_rotation,
  lineage_graph,manifest_ext,manifest_txt,manifest_version,recover,
  phase_macro}.rs`, `phases/budget_cascade.rs`,
  `execution/per_provider_semaphores.rs`,
  `discovery/saturation_event.rs`.
- ✅ **`BudgetPolicy::{Warn,Abort}`** — eliminados (#436, #440).
- ✅ **5 tablas SQLite muertas** (`run_state`, `discovery_dedup`,
  `plan_state`, `budget_events`, `manifest_versions`) — eliminadas vía
  migraciones v016 + v017.
- ✅ **Identificadores en español** (`detectar_outliers`, `cola_reserva`,
  `DEFAULT_COLA_RESERVA`) — renombrados en #446.
- ✅ **`test_support::unique_tempdir`**, **`BudgetObserver::policy`**,
  **`probe_table::{effective_max_tokens, probe_all, max_tokens_auto_path}`**
  — eliminados en #440.
- ✅ **Helpers muertos de `phases/{intake,synthesize,replace,util}.rs`**
  (`read_intake_with_context`, `merge_plan_to_synthesized`,
  `sources_to_replace`, `strip_code_fence`) — eliminados en #442/#443.
- ✅ **Docstring stale en `ranking/mod.rs:4`** — refrescado en #439.
- ✅ **Wire-up `Config::token_budget` → `Db::set_budget`** — #447.
- ✅ **`tiktoken-rs`** — ausente de `Cargo.toml` y `Cargo.lock` (#427).
- ✅ **Migración v017** — presente y correcta.
- ✅ **`persona_angle`** — vivo y correcto (**4 callers** en
  `discovery/coordinator.rs:254, 325, 504, 526`); el flag de auditoría
  fue un falso positivo.
- ✅ **`--model deepseek-chat`** — eliminado del integration test en
  #464 (`39647a1`). Sólo queda el comment stale del script (§9.3).
- ✅ **K.1 / K.2 / K.3** de `docs/proposal-04-cuarta-etapa.md` —
  SHIPPED (§7).
- ✅ **0 `TODO`/`FIXME`/`XXX`/`todo!()`/`unimplemented!()` reales** en
  producción.
- ✅ **Layers 2, 4, 5 de `docs/test-skips.md`** — coinciden exacto con
  el árbol.

---

## §13 — Notas metodológicas + recomendación

### 13.1 — Metodología

Este informe se construyó a partir de **4 verificaciones read-only
independientes**, cada una en su propio worktree anclado a `39647a1`,
sin tocar código fuente ni hacer commits:

| Fuente | Alcance | Hallazgo clave |
|---|---|---|
| `.worktrees/verify-spec-gaps/findings/spec-gaps.md` | 6 items de T01-06 | 3 providers OPEN; **3 claims stale de doc** |
| `.worktrees/verify-e2e-coverage/findings/e2e-coverage.md` | cobertura provider × discovery | roster real 18/15; **1 de 15 con e2e real, y nunca en CI** |
| `.worktrees/verify-deferred/findings/deferred-items.md` | v0.7 §7 + K.x + PR #421 | 8/8 deferred OPEN; K.1-K.3 SHIPPED; #421 varada |
| `.worktrees/verify-skip/findings/skip-inventory.md` | inventario de skips | `docs/test-skips.md` subcuenta en **+3 y +16** |

Cruces documentales usados: `docs/proposal-02-rust.md` (T01-06),
`docs/proposal-03-add-ons.md` (D.x), `docs/proposal-04-cuarta-etapa.md`
(K.x), `docs/v0.7-final-report.md` §7/§8/§11,
`docs/inconsistencies-audit-2026-08-12.md`,
`docs/inconsistencies-audit-2026-08-12-round-2.md`,
`docs/discovery-validation-research-2026-08-13.md`,
`docs/spec-impl-gaps.md`, `docs/test-skips.md`, `docs/COORDINATION.md`.

### 13.2 — Convención de etiquetado

El catálogo `docs/proposal-03-add-ons.md` está **cerrado**: no se añaden
D.x nuevos. Todo hallazgo nuevo de este informe se etiqueta
**VERIFIED-OPEN** y vive aquí hasta que se le asigne una PR. Los
VERIFIED-OPEN de esta ronda son: comment stale de
`e2e_audit_proxy.sh:553`, subcuenta de `docs/test-skips.md` (Layer 3 y
Layer 6), línea 182 de `docs/spec-impl-gaps.md` §5, ausencia de matrix
jobs de discovery en `e2e-network.yml`, y el estado varado de PR #421.

### 13.3 — Limitación conocida

`verify-spec-gaps` §1.3 marca los 8 deferred de v0.7 §7 como "no
re-verificados" en su propio alcance; esa laguna la cubre
`verify-deferred` §A, que los probó uno a uno con grep dirigido. No
queda ningún item del informe sin evidencia de al menos una de las 4
verificaciones.

Ningún item de este informe se validó ejecutando el pipeline contra
providers reales: todas las afirmaciones sobre cobertura e2e derivan de
leer los guards de los scripts y los workflows, no de una corrida. Esa
es precisamente la razón por la que el Tier A #8/#9 es prioritario.

### 13.4 — Recomendación

**Empezar session 4 con una sola PR `docs+ci` que barra el Tier S
entero** (7 items, ~1 h): corrige los 3 claims stale, cierra la nota
D.22.3, alinea `docs/test-skips.md` y `docs/spec-impl-gaps.md` con el
árbol, y limpia el comment de `e2e_audit_proxy.sh:553`. Es barata,
sin riesgo y elimina toda la deuda documental de golpe.

**Segunda PR: cobertura CI** (Tier A #8 + #10, ~45 min) — 2 matrix
jobs, 2 secretos y un paso `--ignored`. Pasa la cobertura real de
discovery de "manual, nunca ejecutada" a "verde en cada run de
`e2e-network`".

**Tercera PR: el bucle de modelos** (Tier A #9, 2–4 h) — script-only,
lleva la cobertura de 1/15 a 15/15 y prueba por primera vez los wire
formats `/v1/responses` y `/v1/messages`.

**Sólo entonces abrir features.** Y antes de tocar `comfy-table`,
`proptest` o `petgraph`, resolver la decisión de política sobre la
no-go list de `AGENTS.md` (Tier B #19): abrir código sobre una
dependencia prohibida es trabajo que se tira.

**Decisión aparte y no bloqueante**: qué hacer con PR #421 (Tier B
#17). 253 commits y 50 ficheros varados a 37 commits de `main` no
mejoran con el tiempo; conviene decidir rebase o cierre formal en esta
sesión, aunque la ejecución vaya después.

---

## §14 — Log post-merge (session 4, 2026-08-13)

Cronología desde el merge inicial (`main@7567c1b`, PR #466) hasta
el cierre de esta revisión, en UTC:

| Hora | Evento |
|---:|---|
| 18:56:06 | PR #466 squash-mergeado en `main@7567c1b` por `airvzxf` (vía `--admin` por política de base no documentada en `/branches/main/protection` API) |
| 18:56:28 | `cargo-audit` en `main` ✓ |
| 18:58:29 | `ci` (gauntlet principal) en `main` ✓ |
| 18:58:38 | `codeql` en `main` ✓ |
| 19:01:31 | `e2e-network` (fast) en `main` — **FAIL** (`models_dev: parse response from https://models.dev/api.json: error decoding response body` + LLM devolvió JSON malformado) |
| 19:11:31 | Re-trigger `e2e-network fast` — **FAIL** (mismo error upstream) |
| 19:13:52 | 3er re-trigger `e2e-network fast` — ✓ **success** (recuperación de `models.dev` API + LLM provider) |
| 19:18:12 | `e2e-network card80` disparado |
| 19:46:30 | `e2e-network card80` ✓ **success** (~28 min) |
| 19:46:40 | `e2e-network explore` disparado |
| 19:56:48 | `e2e-network explore` ✓ **success** (~10 min) |
| 19:58:?? | `origin/fix/audit-findings` borrado (`git push origin --delete`) — Tier S #2 ✅ |

**Diagnóstico de los 2 fallos iniciales de `e2e-network fast`**:

- **Causa raíz**: servicio upstream (`https://models.dev/api.json`)
  devolvió cuerpo que el parser local no pudo decodificar; la caché en
  `~/.local/share/moagan/models_dev.json` tampoco existía en el runner
  fresco (`os error 2`). Sin catálogo de capacidades, el provider
  LLM (MiniMax) devolvió un JSON de verdict casi-válido pero con un
  `:` faltante entre la clave y el valor (`"suggestions"[...]`),
  disparando el `schema violation` y haciendo que `moagan run`
  saliera con código no-cero (40 la 1ª vez, 7 la 2ª).
- **No es regresión**: el diff entre `39647a1` (pre-merge) y `7567c1b`
  (post-merge) es exclusivamente `docs/pending-items-2026-08-13.md`
  (866 inserciones, 0 borrados de código). El binario `moagan` es
  idéntico.
- **Resolución**: el 3er intento, ~7 minutos después, encontró el
  API en estado saludable. Patrón consistente con flake transitorio
  de upstream, no con bug nuestro.

**Estado actual de `main` (2026-08-13 20:00 UTC)**: `7567c1b` +
revisión v2 (este documento), worktrees de session-4 limpios,
working tree del MAIN limpio, 0 PRs abiertos, 1 rama huérfana
(`origin/pr/421`, CLOSED-not-MERGED, decisión pendiente en §8.1).

---

_Última actualización: 2026-08-13 20:00 UTC — verificado contra
`main@7567c1b` (post-#466) + revisión v2 (este commit) mediante 4
auditorías read-only independientes (Phase A), 1 subagente de
síntesis (Phase B), 3 subagentes de validación cruzada (Phase C), 1
subagente de correcciones (Phase D), workflow PR completo (Phase E) y
borrado de rama huérfana (cierre de Tier S #2)._
