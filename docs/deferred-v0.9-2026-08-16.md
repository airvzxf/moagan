---
title: Deferred items — v0.9 (2026-08-16)
date: 2026-08-16
tag: v0.9.0
status: deferred
cancels_from: pending-items-2026-08-13.md §3.1 + §v0.9 prospect
---

# Deferred items — v0.9 (2026-08-16)

> **Fecha**: 2026-08-16
> **HEAD verificado**: `2052b08` (`main`, post-v0.8.0)
> **Origen autoridad**: `docs/proposal-01-concept.md` (V4) >
> `docs/proposal-02-rust.md` (T01-06, **normativo**) >
> `docs/proposal-03-add-ons.md` (catálogo D.x, opt-in).

Este documento formaliza los items del backlog v0.9 prospect que se
**decide NO atacar** en la sesión actual, con la justificación
operativa de cada uno. Son decisiones del operador, no del agente.

---

## §1 — Items NO HACER (decisión del operador)

### 1.1 — Providers GLM / Qwen / Kimi — **NO HACER**

**Spec origen**: `docs/proposal-02-rust.md §0.2` líneas 73-76
(`src/llm/glm.rs`, `src/llm/qwen.rs`, `src/llm/kimi.rs`).

**Estado actual** (per `docs/pending-items-2026-08-13.md §3.1`):

- 0/3 implementados (sólo entries de capability-lookup y test asserts).
- `verify-spec-gaps §1.3` confirmó ausencia de código de producción.

**Razón de la decisión**:

1. El operador **no tiene API keys** de GLM, Qwen, ni Kimi.
2. Para adquirirlas hay que contratar planes pay-as-you-go con esos
   proveedores, lo cual no se va a hacer en el corto plazo.
3. Sin tickets concretos ni demanda operativa, implementar 3 providers
   nuevos sería trabajo que se tira.

**Implicaciones**:

- Los 3 providers siguen ausentes del binario. Los catalog entries
  de capability (`src/llm/capability.rs:295`, `:427`) y los asserts
  de test (`src/llm/response_format_opt_out.rs:150-151`) permanecen
  como stub para uso futuro.
- Si en el futuro el operador contrata API keys, los 3 providers
  pueden re-habilitarse siguiendo el patrón de los 6 providers
  actuales (`deepseek`, `minimax`, `opencode_go` + 2 sub-wires,
  `mock`).

**Status**: `DEFERRED v0.9+ (sin fecha)`.

---

### 1.2 — Cross-platform sandbox fallback (macOS / WSL) — **NO HACER**

**Spec origen**: `docs/proposal-02-rust.md §11.4` (asume Linux).

**Estado actual** (per `docs/handoff-next-session.md §3.2`):

- Sandbox hardening (Tracks E + D.11.x) es Linux-only.
- Las primitivas `unshare(CLONE_NEW*)` + cgroup v2 + seccomp BPF
  no tienen equivalente directo en macOS ni WSL.
- `src/sandbox/{process,allowlist}.rs` usan `nix` crate y BPF, ambos
  específicos de Linux kernel.

**Razón de la decisión**:

1. El operador declara que su **único público objetivo son archivos
   Linux puros** (no macOS, no Windows).
2. Implementar fallbacks para macOS/WSL sería:
   - Mac: sandbox-exec / Seatbelt APIs (alto costo de implementación).
   - WSL: depende del host Windows + WSL version (alto costo de QA).
3. Sin demanda del público objetivo, el costo no se justifica.

**Implicaciones**:

- El binario `moagan` **sólo soporta Linux** (x86_64-gnu, x86_64-musl,
  aarch64-gnu). El `pre-commit lefthook` y los CI workflows siguen
  corriendo sólo en runners Linux.
- Si en el futuro se necesita soporte macOS/WSL, los fallbacks
  requerirán un PR sustancial con primitivas OS-specific nuevas.

**Status**: `DEFERRED v0.9+ (sin fecha)`.

---

## §2 — Items que SÍ se hacen en v0.9

Por orden de complejidad ascendente (decisión del operador, sesión
2026-08-16):

| # | Item | Coste | Notas |
|---:|---|---:|---|
| 1 | SaturationSink registry wiring | 0.5 d | follow-up de #494 — wire `Telemetry` handle a `BreakeredProvider` |
| 2 | `process_locks` lease module (D.1.5) | 1 d | schema v008 existe; falta `src/storage/lease.rs` con FencingToken |
| 3 | HardIncompat extensions | 1 d | opt-in catálogo I.6 (`ClusterLocalInGlobal`, `PullInPushOnly`, `StatelessInStateful`) |
| 4 | AsyncEmbedder trait | 1 d | follow-up de #496 — bridge sync `Embedder` ↔ async `RemoteEmbedder` |
| 5 | `proptest` for hashes | 2 d | dev-deps only per ADR-0001 |
| 6 | Dashboard cross-run analytics | 2 d | `/api/compare-runs`, `/api/aggregates` |
| 7 | K.4 PDF parser | 3 d | decisión `lopdf` vs `pdftotext` shelling out |
| 8 | `petgraph` DAG backend | 3 d | gated `feature = "dag"`, sólo `deep` mode per ADR-0001 |

**Total estimado**: ~13 d (~2-3 sprints de 5 d).

**Política de aborto** (per operador):

> "Si alguno es tan complicado que empieza a destruir funcionalidad
> código revolverlo o empieza a tomar mucho más tiempo de lo que podría
> tomar se aborta ese ítem o característica y se documenta, se eliminan
> los cambios, restaura todo, se elimina la rama y se continúa con los
> otros."

Aplicar esta política a cada item: si el sub-PR rebasa 1.5x el
estimado de coste o rompe tests existentes, abortar, documentar la
razón, descartar rama, continuar con el siguiente.

---

## §3 — Items que siguen DEFERRED desde antes (no nuevos)

Per `docs/handoff-next-session.md §3` + `docs/v0.7-final-report.md §7`:

- **Multimodal streaming** (B#21) — bloqueado upstream.
- **`comfy-table` for CLI tables** (D.14.23) — DEFERRED per
  ADR-0001 (no benefit > cost en v0.9).
- **HardIncompat extensions** beyond catálogo I.6 — opt-in catálogo.

Estos items NO entran en scope de v0.9.

---

## §4 — Cross-check con otros docs

Este documento **reemplaza** las menciones vagas de "GLM/Qwen/Kimi
pendiente" y "cross-platform sandbox deferred" en:

- `docs/pending-items-2026-08-13.md §3.1` (3 providers OPEN).
- `docs/handoff-next-session.md §3.2` (cross-platform sandbox).
- `docs/handoff-session-4-2026-08-16.md` (si mencionaba estos items
  como prospect, ya están documentados aquí como NO HACER).

Los items "OPEN" en `pending-items-2026-08-13.md §3.1` se mantienen
históricamente — ese informe es de 2026-08-13 (sesión 3) y refleja
el estado de ese momento. La decisión de NO HACER está formalizada
aquí con fecha 2026-08-16.

---

_Última actualización: 2026-08-16._
