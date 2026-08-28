---
id: T01-06
temperature: 0.1
rep: 6
response_id: 06b3a1c2e0f89585c3a223685420d352
input_tokens: 58
output_tokens: 28198
cache_read_input_tokens: 16768
cache_creation_input_tokens: 0
stop_reason: end_turn
status: ok
timestamp_utc: 2026-07-25T08:14:11.210415+00:00
---

# Especificación Técnica Implementable — `moagan` (MoA Orchestrator en Rust)

> **Status (2026-08-28):** This is the **T01-06 frozen spec** (synthesis_date
> 2026-07-25, response_id `06b3a1c2`). It is the **normative Rust
> implementation spec** per `AGENTS.md` ("T01-06 wins" on conflicts), but
> the **live state** has diverged in several places:
>
> - The Cargo.toml pin (version 0.12.14, AGPL-3.0-or-later, current
>   deps including rustls, r2d2, rusqlite) — see `Cargo.toml`.
> - The src/ tree (additions: `audit/`, `coverage/`, `ranking/`,
>   `atomic/`, `sandbox/`, `redact/`; renames:
>   `ProviderRegistry` → `ProviderPool` in v0.10).
> - The §0.5 decisions table (some entries stale, e.g. cache hash
>   now BLAKE3 not sha256, retention 30d not 365d).
> - The env vars (`MOAGAN_LOG_FORMAT`, `MOAGAN_DECISION_FORMAT`,
>   `MOAGAN_LOG_TO_STDERR` were added in v0.10/v0.12).
>
> The **normative claim** is that the *decisions* (the §0.5 table and
> the architectural mandates throughout) remain authoritative; the
> *implementation details* (Cargo pin, src/ tree, env var table) are
> now historical. See [`docs/cli-cheatsheet.md`](../cli-cheatsheet.md)
> for the live CLI surface and `Cargo.toml` for the live pin.

## 0. Decisiones globales de arquitectura

Antes de aterrizar cada componente, fijo las decisiones que afectan a todo el sistema. Cada una proviene de la propuesta `14-integrada-v4.md` y se justifica aquí para evitar re-imaginación por parte del siguiente modelo.

### 0.1. Estilo de proceso

- **Un binario, un proceso por `run`.** El CLI lanza un proceso que vive mientras el run está activo. No hay daemon persistente fuera del dashboard de telemetría (que es un subproceso opcional de corta vida).
- **Estado mutuo en memoria + sidecar en disco.** Cada fase escribe su artefacto en disco antes de retornar. Reinicio del proceso ⇒ reanuda desde disco.
- **Idempotencia por hash.** Toda llamada LLM se identifica por `hash(input_completo + parámetros_llm)`. Si ya existe `output` con ese hash, se reusa sin re-llamar. Esto cubre caché intra-run y, opcionalmente, cross-run.

### 0.2. Layout de directorios (código fuente)

> **Estado (2026-08-28):** §0.2 y §0.3 describen el árbol de directorios y
> el `Cargo.toml` pin tal como estaban cuando T01-06 se congeló
> (synthesis_date 2026-07-25). El árbol real ha divergido
> significativamente — directorios como `src/audit/`, `src/coverage/`,
> `src/ranking/`, `src/atomic/`, `src/sandbox/`, `src/redact/`, etc.
> se añadieron después; `src/prompts/` nunca existió; muchos providers
> se consolidaron en `src/llm/openai_compatible.rs` /
> `openai_compat.rs` / `minimax.rs` / `deepseek.rs` / `mock.rs` /
> `kimi.rs` / `opencode.rs`. **Esta sección se conserva como
> referencia histórica del spec original; no refleja la
> implementación actual.** El árbol vivo está en `src/` y el pin vivo
> está en `Cargo.toml` (v0.12.14 al día de hoy; ver
> [`docs/cli-cheatsheet.md`](../cli-cheatsheet.md) §0.2 para la
> tabla de env vars vigente).

```text
moagan/
├── Cargo.toml
├── Cargo.lock
├── .env.example
├── README.md
├── src/
│   ├── main.rs
│   ├── lib.rs
│   ├── config.rs
│   ├── error.rs
│   ├── fs_layout.rs
│   ├── ids.rs
│   ├── time.rs
│   ├── cancel.rs
│   ├── domain/
│   │   ├── mod.rs
│   │   ├── run.rs
│   │   ├── brief.rs
│   │   ├── constraint.rs
│   │   ├── sketch.rs
│   │   ├── proposal.rs
│   │   ├── critique.rs
│   │   ├── validation.rs
│   │   ├── evaluation.rs
│   │   ├── tag.rs
│   │   ├── cluster.rs
│   │   ├── facet.rs
│   │   ├── contradiction.rs
│   │   └── matrix.rs
│   ├── ingest/
│   │   ├── mod.rs
│   │   ├── normalize.rs
│   │   ├── detect.rs
│   │   └── budget.rs
│   ├── llm/
│   │   ├── mod.rs
│   │   ├── provider.rs         (trait)
│   │   ├── http.rs             (cliente HTTP genérico, reqwest)
│   │   ├── retry.rs
│   │   ├── budget.rs
│   │   ├── cache.rs            (SQLite + filesystem)
│   │   ├── request.rs          (schemas de request por provider)
│   │   ├── response.rs         (schemas de response + parse de usage)
│   │   ├── minimax.rs
│   │   ├── glm.rs
│   │   ├── qwen.rs
│   │   ├── kimi.rs
│   │   ├── deepseek.rs
│   │   ├── opencode_go.rs
│   │   └── mock.rs
│   ├── redact/
│   │   ├── mod.rs
│   │   ├── patterns.rs
│   │   └── apply.rs
│   ├── phases/
│   │   ├── mod.rs
│   │   ├── phase.rs            (trait)
│   │   ├── pipe.rs             (Pipeline executor)
│   │   ├── scheduler.rs
│   │   ├── intake.rs
│   │   ├── clarify.rs
│   │   ├── route.rs
│   │   ├── decompose.rs
│   │   ├── sketch_phase.rs
│   │   ├── proposal.rs
│   │   ├── gate.rs
│   │   ├── validate.rs
│   │   ├── critique.rs
│   │   ├── repair.rs
│   │   ├── judge.rs
│   │   ├── rank.rs
│   │   ├── cluster_proposals.rs
│   │   ├── synthesize.rs
│   │   ├── discover_matrix.rs
│   │   ├── discover_tag.rs
│   │   ├── discover_cluster.rs
│   │   ├── discover_contradict.rs
│   │   ├── discover_facet.rs
│   │   ├── discover_extract.rs
│   │   ├── discover_integrate.rs
│   │   └── deliver.rs
│   ├── discovery/
│   │   ├── mod.rs
│   │   ├── matrix.rs
│   │   ├── tagger.rs
│   │   ├── clusterer.rs
│   │   ├── contradiction.rs
│   │   ├── facet.rs
│   │   ├── extractor.rs
│   │   └── integrator.rs
│   ├── execution/
│   │   ├── mod.rs
│   │   ├── parallelism.rs
│   │   ├── timeout.rs
│   │   └── checkpoint.rs
│   ├── validators/
│   │   ├── mod.rs
│   │   ├── structural.rs
│   │   ├── constraints.rs
│   │   ├── rust_validator.rs
│   │   ├── python_validator.rs
│   │   ├── typescript_validator.rs
│   │   ├── sql_validator.rs
│   │   └── schema_validator.rs
│   ├── sandbox/
│   │   ├── mod.rs
│   │   ├── process.rs
│   │   └── allowlist.rs
│   ├── context/
│   │   ├── mod.rs
│   │   ├── resolver.rs
│   │   └── loader.rs
│   ├── telemetry/
│   │   ├── mod.rs
│   │   ├── ring_run.rs
│   │   ├── ring_phase.rs
│   │   ├── ring_call.rs
│   │   ├── retention.rs
│   │   ├── redact.rs
│   │   ├── export.rs
│   │   └── dashboard.rs        (subcomando)
│   ├── storage/
│   │   ├── mod.rs
│   │   ├── sqlite.rs
│   │   ├── artifacts.rs
│   │   ├── compression.rs
│   │   └── run_fs.rs
│   ├── checkpoint/
│   │   ├── mod.rs
│   │   └── human.rs
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── run.rs
│   │   ├── continue_cmd.rs
│   │   ├── resume.rs
│   │   ├── rerun.rs
│   │   ├── inspect.rs
│   │   ├── import.rs
│   │   └── telemetry_cmd.rs
│   └── prompts/
│       ├── mod.rs
│       ├── registry.rs
│       ├── intake_v1.md
│       ├── clarify_v1.md
│       ├── sketch_v1.md
│       ├── proposal_v1.md
│       ├── critic_v1.md
│       ├── judge_v1.md
│       ├── tagger_v1.md
│       ├── facet_v1.md
│       ├── extractor_v1.md
│       ├── integrator_v1.md
│       └── ... (uno por rol)
└── tests/
    ├── fixtures/
    ├── integration_mvp.rs
    ├── integration_discovery.rs
    └── unit/
```

### 0.3. Dependencias pin

> **Estado (2026-08-28):** §0.3 describe el `Cargo.toml` pin tal
> como estaba cuando T01-06 se congeló (synthesis_date 2026-07-25).
> El pin vivo en `Cargo.toml` ha divergido significativamente —
> la versión actual es **0.12.14** (no 0.4.0), la licencia es
> **AGPL-3.0-or-later** (no MIT/Apache-2.0), y se han añadido/
> quitado varias dependencias (rustls en lugar de native-tls, r2d2
> + rusqlite en lugar de sqlx, etc.). **Esta sección se conserva
> como referencia histórica del spec original; no refleja la
> implementación actual.** El pin vivo está en `Cargo.toml` (ver
> también `AGENTS.md` §"Differentiated allow-list" para la
> política de crates admitidos).

```toml
[package]
name = "moagan"
version = "0.4.0"      # historical: T01-06 v0.4 spec time
                       # current:  0.12.14 (see Cargo.toml)
edition = "2024"
rust-version = "1.97.1"
license = "MIT OR Apache-2.0"  # historical: T01-06 v0.4 spec time
                                # current:  AGPL-3.0-or-later (see Cargo.toml)

[dependencies]
tokio = { version = "1.40", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", features = ["preserve_order"] }
serde_yaml = "0.9"
toml = "0.8"
uuid = { version = "1.10", features = ["v7", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "1"
anyhow = "1"
clap = { version = "4.5", features = ["derive", "env"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "gzip", "stream"] }
rustls = "0.23"
sha2 = "0.10"
blake3 = "1.5"
hex = "0.4"
regex = "1.10"
once_cell = "1.19"
parking_lot = "0.12"
dashmap = "6.1"
rusqlite = { version = "0.32", features = ["bundled", "json1"] }
r2d2 = "0.8"
r2d2_sqlite = "0.24"
walkdir = "2.5"
flate2 = "1.0"
tar = "0.4"
zip = "2.2"
zstd = "0.13"
indicatif = "0.17"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
dotenvy = "0.15"
dialoguer = { version = "0.11", default-features = false }
rayon = "1.10"
fastrand = "2.0"
memchr = "2.7"
once_cell = "1.19"
directories = "5.0"
async-trait = "0.1"
futures = "0.3"
# Validator embed para JSON Schema (opcional, justificado en §0.4)
jsonschema = { version = "0.17", default-features = false }

[dev-dependencies]
tempfile = "3.10"
assert_fs = "1.1"
predicates = "3.1"
wiremock = "0.6"
insta = { version = "1.39", features = ["yaml"] }
```

### 0.4. Justificación de cada dependencia relevante

- **`reqwest` con `rustls-tls`**: el enunciado prohíbe SDKs de Anthropic; usamos HTTP genérico. `rustls-tls` evita la dependencia nativa de OpenSSL en macOS/Linux.
- **`rusqlite` + `r2d2_sqlite`**: SQLite embebido, con pool para escrituras concurrentes desde el dashboard de telemetría.
- **`tokio` con `full`**: el sistema es I/O-bound (HTTP, SQLite, filesystem, sandbox processes). Runtime unificado.
- **`clap` con `derive`**: estructura del CLI consistente.
- **`jsonschema`**: validación de los JSON outputs de los LLM contra los schemas declarados en §4. Forma parte del "gate estructural".
- **`tracing` + `tracing-subscriber`**: telemetría estructurada, mismo formato que `telemetry/phases.jsonl`.
- **`dialoguer`**: prompts interactivos para checkpoints humanos y `switch-api-key`.
- **`flate2` + `zstd` + `tar` + `zip`**: export y compresión configurables.
- **`uuid` v7**: `run_id` ordenado por tiempo (UUID v7, definido en RFC 9562).
- **`memchr` + `regex`**: redacción eficiente.
- **`directories`**: resuelve `XDG_DATA_HOME` / `~/.local/share` para `.runs/`.
- **`dotenvy`**: carga `.env` en desarrollo (`.env.example` versionado).
- **`indicatif`**: barras de progreso para runs largos de `discovery`.

### 0.5. Decisiones tomadas sobre ambigüedades de la propuesta

| # | Ambigüedad en la propuesta | Decisión | Razón |
|---|---|---|---|
| 1 | "Sketches de 400–800 tokens" sin tope duro para outputs de LLM | `max_tokens` uniforme por rol en `prompts/registry.rs`; todos los roles usan 1_000_000 como techo. | Evita truncamientos ambiguos con un techo uniforme para todos los roles. |
| 2 | "Tagger ligero" sin parámetros | Tagger: `temperature=0`, `top_p=0.2`, `max_tokens=1_000_000`, JSON mode forzado. | Determinismo + techo uniforme para todos los roles. |

> **Estado (2026-08-28) — rows 1 + 2:** el `1_000_000` uniforme fue
> válido en v0.6 pero en v0.10+ el techo es per-`(provider, model)`,
> auto-descubierto en runtime y persistido en `max_tokens_auto.toml`
> (ver [`docs/max-tokens-auto.md`](../max-tokens-auto.md)). El path
> `prompts/registry.rs` tampoco existe como tal; la lógica vive
> ahora en `src/llm/prompts/` (varios módulos) más los defaults por
> rol en `src/phases/phase.rs`. La **decisión** (uniformidad por rol)
> sigue vigente; la **implementación** (1M uniforme vía
> `prompts/registry.rs`) es histórica.
| 3 | "Embedding ligero" para clustering | **No se descargan modelos**. Clustering usa `hash_lsh` sobre texto tokenizado (SimHash 64-bit) + segunda pasada con LLM sólo si se piden `cluster_label` y `cluster_summary`. Embeddings locales (fastText) son demasiado pesados para MVP; se documenta como mejora. | Mantiene binario sin assets externos. |
| 4 | Forma de "JSON mode forzado" en providers que no lo soportan | Proveedor implementa `supports_json_mode()`; si false, prompt indica `Responde únicamente con un JSON válido. Sin texto fuera del JSON.` y se valida con `jsonschema`. | Portabilidad. |
| 5 | "Sin seeds" pero hace falta reproducibilidad | Sistema detecta duplicados por hash del input completo (prompt + parámetros_llm + fase + modelo). No garantiza misma salida, sí garantiza que no se duplican inputs. | Honra la propuesta. |
| 6 | "Una sola transacción SQLite por operación" | Operaciones multi-tabla usan `BEGIN IMMEDIATE` con rollback explícito. El dashboard hace `SELECT` sólo, fuera de transacciones. | Atomicidad. |
| 7 | "Sidecar JSON por artefacto" | Todo artefacto del run se persiste como JSON o Markdown; el binario no retiene estado que no esté en disco. | Recuperabilidad. |
| 8 | "Timeouts `0` = infinito" | Implementado literalmente: si `timeout == 0`, la duración se salta. Warning en `manifest.json` cuando se detecta. | Literal. |
| 9 | "Paralelismo: `min(solicitado, max_parallelism - en_uso)`" | Implementado con `Semaphore` en `execution/parallelism.rs`. Una sola `Semaphore` global por proceso. | Garantiza tope absoluto. |
| 10 | "Provider switches preservan sketches" | El switch nunca borra artefactos previos. Cambia la entrada de `provider_changes` en `manifest.json` y la fila `provider_changes` en SQLite. | Honra la propuesta. |
| 11 | "Run con cardinalidad 500+ sketches" | Pipeline de discovery usa `tokio::task` + `Semaphore`; los sketches se escriben a disco apenas estén listos (no se acumulan en memoria). | Memoria acotada. |
| 12 | "Detección de outliers" | SimHash + Manhattan distance sobre 64-bit. Si `min_distance > umbral` (default 32), es outlier. | Sin embeddings. |
| 13 | "Checkpoints humanos sin timeout" | `dialoguer::Input` no envuelto en `tokio::time::timeout`. | Literal. |
| 14 | "Switch provider mid-run" | Nuevo provider se aplica a la siguiente llamada; la llamada en curso termina con el provider anterior. | No hay aborto. |
| 15 | "Telemetry redact_on_write" | Redacción se aplica en el método `write` de `telemetry::*`; cualquier fuente que escriba telemetría pasa por `redact::apply`. | Honra la propuesta. |
| 16 | "Discovery sin DAG" | `discovery` no usa `phases/decompose.rs`. | Honra la propuesta. |
| 17 | "Clustering en discovery: similitud coseno" | Cambiado a SimHash (decisión #3). La "similitud coseno" se mantiene como capa opcional post-clustering cuando se genera `cluster_summary` con LLM. | Sin embeddings. |
| 18 | "Descomposición DAG sólo en deep" | El trait `DagNode` existe pero sólo `deep` lo usa. | Honra la propuesta. |
| 19 | "Sidecar JSON por run completo" | `.runs/<run_id>/manifest.json` es la fuente canónica de parametrización. SQLite es índice consultable. | Single source of truth. |
| 20 | "Switch api-key interactivo" | Si el usuario no pasó `--switch-api-key`, el CLI pregunta con `dialoguer`. `env:` o `file:` evitan la pregunta. | Honra la propuesta. |

---

## 1. Modelo de datos y persistencia

### 1.1. Regla de oro: "el archivo manda, SQLite indexa"

El directorio `.runs/<run_id>/` contiene la verdad operacional. SQLite es el índice que permite listar, filtrar y agregar. Cualquier inconsistencia entre ambos se resuelve a favor del filesystem.

Esto implica:

- **Antes de cada `INSERT` en SQLite, el archivo sidecar debe existir.** Si la escritura del sidecar falla, la fila no se crea.
- **Las migraciones de SQLite** se numeran con timestamp (`v001_initial.sql`, `v002_provider_changes.sql`) y se ejecutan en `BEGIN` por archivo. El número de versión se guarda en `PRAGMA user_version`.
- **Las queries que el dashboard sirve** son sólo `SELECT` con `WHERE run_id = ?`. No hay mutaciones desde el dashboard.

### 1.2. Estructura del filesystem

```text
${MOAGAN_HOME:-~/.local/share/moagan}/.runs/<run_id>/
├── manifest.json                # Parametrización + estado lógico
├── brief.json                   # CanonicalBrief
├── problem_graph.json           # Sólo si mode ∈ {deep, batch}
├── exploration_matrix.json      # Sólo si mode == discovery
├── sketches/
│   ├── sk_<uuid7>.json          # Sketch completo (incluye output LLM)
│   └── ...
├── tags/
│   └── sk_<uuid7>.json          # Tags del sketch
├── clusters/
│   ├── cluster_<n>.json
│   └── clusters_index.json
├── contradictions/
│   └── contradictions.json
├── facets/
│   └── cat_<id>_facets.json     # Lista de facetas por categoría
├── extractions/
│   └── cat_<id>/
│       ├── faceta_<slug>.md
│       └── ...
├── drafts/
│   └── cat_<id>/
│       ├── borrador.md
│       └── issues.json
├── proposals/
│   ├── p_<uuid7>.json
│   └── ...
├── critiques/
│   ├── p_<uuid7>_critic_<role>.json
│   └── ...
├── revisions/
│   ├── p_<uuid7>_rev_<n>.json
│   └── ...
├── validation/
│   └── p_<uuid7>.json
├── evaluations/
│   └── p_<uuid7>.json
├── rankings/
│   ├── ranking.json
│   └── pareto.json
├── cluster_proposals/
│   └── clusters.json
├── syntheses/
│   └── syn_<uuid7>.json
├── final/
│   ├── cat_<id>.md              # Modo discovery
│   ├── uncategorized.md         # Opcional
│   ├── summary.md
│   ├── portfolio.md             # Modos fast/standard/deep/explore/batch
│   ├── recommendation.md
│   └── deltas.md
├── logs/
│   ├── intake.log
│   ├── clarify.log
│   └── ...
├── telemetry/
│   ├── run.json
│   ├── phases.jsonl.gz
│   ├── calls.jsonl.gz
│   ├── provider_usage.json
│   ├── timeline.html
│   └── dashboard.html
├── checkpoints/
│   ├── ckp_01.json
│   └── ...
└── cache/
    └── llm/
        └── <hash>.json
```

### 1.3. Convenção de nombrado

- `run_id`: UUID v7 (`018f3a2b-7c9d-7e8f-9b2a-4c5d6e7f8a9b`).
- `sk_id`, `p_id`, `critic_id`, `rev_id`, `syn_id`: UUID v7 independientes, pero referenciados desde `manifest.json` y desde las filas SQLite.
- `cat_<id>`: entero incremental (`cat_01`, `cat_02`…) asignado por orden de creación durante tagging.
- `cluster_<n>`: entero incremental.
- `faceta_<slug>`: slug kebab-case derivado del nombre de la faceta.

### 1.4. Versionado de artefactos

Cada JSON tiene un campo `schema_version: "v1"`. Cualquier cambio incompatible requiere un nuevo schema (`v2`) y un script de migración opcional (`tools/migrate_v1_to_v2.rs`). Por ahora todo es `v1`.

### 1.5. Sidecars `.json` vs `.jsonl`

- **Un artefacto por archivo `.json`**: sketches, proposals, critiques, evaluations, runs.
- **Stream `.jsonl.gz`**: fases (event append-only) y calls (event append-only). Append-only, una línea por evento.

Razón: `jsonl.gz` es append-only (no requiere reescribir el archivo completo al añadir un evento) y `gzip` permite búsquedas eficientes con `zcat | grep`.

---

## 2. SQLite: schema exacto y orden de mutaciones

### 2.1. Schema (literal, sin más tablas)

```sql
-- v001_initial.sql
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS runs (
  run_id          TEXT PRIMARY KEY,
  parent_run_id   TEXT,
  shared_brief_hash TEXT,
  mode            TEXT NOT NULL CHECK (mode IN ('fast','standard','deep','explore','batch','discovery')),
  created_at      TEXT NOT NULL,
  started_at      TEXT,
  ended_at        TEXT,
  status          TEXT NOT NULL CHECK (status IN ('created','running','paused','completed','timeout','cancelled','failed')),
  root_dir        TEXT NOT NULL,
  FOREIGN KEY (parent_run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);
CREATE INDEX IF NOT EXISTS idx_runs_created ON runs(created_at DESC);

CREATE TABLE IF NOT EXISTS run_siblings (
  run_id          TEXT NOT NULL,
  sibling_run_id  TEXT NOT NULL,
  PRIMARY KEY (run_id, sibling_run_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id),
  FOREIGN KEY (sibling_run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS run_context_refs (
  run_id          TEXT NOT NULL,
  context_ref     TEXT NOT NULL,
  context_type    TEXT NOT NULL CHECK (context_type IN ('run_id','path','dir')),
  PRIMARY KEY (run_id, context_ref),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS provider_changes (
  run_id          TEXT NOT NULL,
  from_provider   TEXT NOT NULL,
  from_plan_id    TEXT,
  to_provider     TEXT NOT NULL,
  to_plan_id      TEXT,
  at              TEXT NOT NULL,
  actor           TEXT NOT NULL CHECK (actor IN ('user','auto_pause','auto_budget')),
  PRIMARY KEY (run_id, at),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS provider_usage (
  run_id          TEXT NOT NULL,
  provider        TEXT NOT NULL,
  plan_id         TEXT,
  calls           INTEGER NOT NULL DEFAULT 0,
  tokens_total    INTEGER NOT NULL DEFAULT 0,
  errors          INTEGER NOT NULL DEFAULT 0,
  started_at      TEXT,
  ended_at        TEXT,
  PRIMARY KEY (run_id, provider, plan_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS phases (
  run_id          TEXT NOT NULL,
  phase_name      TEXT NOT NULL,
  event           TEXT NOT NULL CHECK (event IN ('start','end','error','cancel')),
  at              TEXT NOT NULL,
  details_json    TEXT,
  PRIMARY KEY (run_id, phase_name, at),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_phases_run ON phases(run_id, at);

CREATE TABLE IF NOT EXISTS calls (
  call_id         TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL,
  phase           TEXT NOT NULL,
  provider        TEXT NOT NULL,
  model           TEXT NOT NULL,
  endpoint        TEXT NOT NULL,
  input_tokens    INTEGER NOT NULL,
  output_tokens   INTEGER NOT NULL,
  total_tokens    INTEGER NOT NULL,
  temperature     REAL,
  top_p           REAL,
  role            TEXT NOT NULL,
  duration_seconds REAL NOT NULL,
  http_status     INTEGER,
  status          TEXT NOT NULL CHECK (status IN ('ok','error','timeout','cancelled','truncated')),
  error_code      TEXT,
  error_message   TEXT,
  error_message_redacted TEXT,
  retry_count     INTEGER NOT NULL DEFAULT 0,
  truncated       INTEGER NOT NULL CHECK (truncated IN (0,1)),
  output_hash     TEXT,
  cache_hit       INTEGER NOT NULL CHECK (cache_hit IN (0,1)),
  started_at      TEXT NOT NULL,
  ended_at        TEXT,
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_calls_run ON calls(run_id);
CREATE INDEX IF NOT EXISTS idx_calls_phase ON calls(run_id, phase);
CREATE INDEX IF NOT EXISTS idx_calls_provider ON calls(run_id, provider);

CREATE TABLE IF NOT EXISTS checkpoints (
  run_id          TEXT NOT NULL,
  ckp_id          TEXT NOT NULL,
  phase           TEXT NOT NULL,
  kind            TEXT NOT NULL CHECK (kind IN ('intake','clarify','final','custom')),
  payload_json    TEXT NOT NULL,
  resolved_at     TEXT,
  resolved_action TEXT,
  PRIMARY KEY (run_id, ckp_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
```

### 2.2. Migraciones

`storage/sqlite.rs` mantiene un `Vec<(&str, &str)>`, donde el primer elemento es el nombre (`v001_initial.sql`) y el segundo es el SQL embebido. `user_version` se incrementa con `PRAGMA user_version = N` tras aplicar todas las migraciones hasta `N`.

```text
PRAGMA user_version;
-- 0 al inicio
PRAGMA user_version = 1;
-- tras v001_initial.sql
```

### 2.3. Pool de conexiones

```rust
pub type DbPool = r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>;
pub struct Db { pool: DbPool }

impl Db {
    pub fn open(root_dir: &Path) -> Result<Self> { ... }
    pub fn conn(&self) -> Result<r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>> { ... }
}
```

Las escrituras usan `BEGIN IMMEDIATE` cuando modifican más de una tabla (transaccionales). Las lecturas no usan transacción explícita.

### 2.4. Ubicación del archivo SQLite

```text
${MOAGAN_HOME:-~/.local/share/moagan}/meta.sqlite
```

**Un único SQLite para todos los runs.** WAL permite lecturas concurrentes desde el dashboard.

### 2.5. Orden de mutaciones en cada fase

Para cada fase, el patrón es:

1. **Iniciar**: `telemetry::ring_phase::start` (append `phases.jsonl` + `INSERT INTO phases`).
2. **Ejecutar**: para cada llamada LLM, `telemetry::ring_call::record` (append `calls.jsonl` + `INSERT INTO calls` + `UPDATE provider_usage`).
3. **Persistir artefacto**: escribir sidecar `.json`/`.md` a disco.
4. **Indexar**: `INSERT/UPDATE` en SQLite correspondiente al artefacto.
5. **Finalizar**: `telemetry::ring_phase::end` (append `phases.jsonl` + `INSERT INTO phases`).

Si el paso 3 falla, el paso 4 no se ejecuta; el paso 5 sí, con `event='error'`.

### 2.6. Consistencia eventual vs inmediata

- **Inmediata**: el sidecar existe ⇒ la fila SQLite existe (o existirá en la siguiente fase).
- **Eventual**: `phases.jsonl.gz` puede tener una línea más que la última fila en `phases` si hubo crash. Al iniciar, `moagan continue` reconcilia comparando timestamps y leyendo la última línea del jsonl.

---

## 3. Identificadores, hashing y cache

### 3.1. `run_id`

UUID v7. Generado por `ids::new_run_id()`:

```rust
pub fn new_run_id() -> Uuid {
    Uuid::now_v7()
}
```

### 3.2. Hash canonico de input

Para detectar duplicados y alimentar el cache:

```text
hash_input = sha256(
  role_id || \x1f ||            # "intake" | "sketch" | "judge" | ...
  phase_name || \x1f ||
  brief_hash || \x1f ||
  provider_id || \x1f ||
  model || \x1f ||
  temperature || \x1f ||
  top_p || \x1f ||
  max_tokens || \x1f ||
  prompt_rendered   # prompt final, con placeholders ya sustituidos
)
```

Esto se computa en `llm/cache.rs::key_for(...)`. La implementación:

```rust
pub struct CallKey {
    pub role: String,
    pub phase: String,
    pub brief_hash: String,
    pub provider: String,
    pub model: String,
    pub temperature: f32,
    pub top_p: f32,
    pub max_tokens: u32,
    pub prompt: String,
}

impl CallKey {
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        for part in [
            &self.role, &self.phase, &self.brief_hash,
            &self.provider, &self.model,
            &self.temperature.to_string(), &self.top_p.to_string(),
            &self.max_tokens.to_string(),
            &self.prompt,
        ] {
            h.update(part.as_bytes());
            h.update(&[0x1f]);
        }
        hex::encode(h.finalize())
    }
}
```

### 3.3. Cache LLM

Estructura:

```text
.runs/<run_id>/cache/llm/<hash>.json
```

Contenido:

```json
{
  "key_hash": "abc123...",
  "provider": "minimax",
  "model": "MiniMax-M3",
  "endpoint": "https://api.minimax.io/anthropic/v1/messages",
  "request": { "messages": [...], "temperature": 0.6, "top_p": 0.95, "max_tokens": 1000000 },
  "response": { "raw": "...", "parsed": {...} },
  "usage": { "input_tokens": 123, "output_tokens": 456, "total_tokens": 579 },
  "stored_at": "2026-07-24T10:31:00Z",
  "ttl": null
}
```

Política:

- **Cache hit** = `cache_hit = 1` en `calls`, y la columna `output_hash` apunta al cache.
- **Cache miss** = llamada real; al terminar, se escribe el archivo de cache.
- **Cross-run**: el segundo run con mismo hash puede reusar si la policy es `cross_run=true`. Por default, sólo intra-run.
- **Limpieza**: `cache_hit` se calcula antes de la llamada HTTP; si la respuesta del LLM difiere del cache (tokens !== cache.usage), se reescribe el cache y se marca `error_code=null` con `cache_hit=0` (defensa contra envenenamiento).

### 3.4. Reuso por `context`

Cuando `moagan run --mode deep --context run_disc`:

- `context_resolver` carga el run origen.
- Para cada `extractions/cat_<id>/faceta_<slug>.md`, calcula su `sha256`.
- Los hashes se anexan al `brief_hash` del nuevo run (campo `shared_brief_hash` en `runs`).
- En `runs`, `parent_run_id` se setea con el run origen.

#### 3.4.1. Implementación (v0.3 sub-fase J)

El resolver vive en `src/context/resolver.rs` y exporta tres
tipos / funciones públicas:

- `enum ContextRef { RunId(RunId), FilePath(PathBuf), DirPath(PathBuf) }`
- `fn resolve_classify(input: &str, home: &MoaganHome) -> Result<ContextRef>`
  — clasifica el input. UUID v7 primero (parse barato, sin IO);
  si falla, prueba si es un path que existe. Si no es nada,
  `Error::InvalidArgs`.
- `fn resolve(home: &MoaganHome, raw: &str) -> Result<ContextRef>`
  — además valida que un `RunId` apunte a un directorio
  `<home>/.runs/<id>/` que existe.

El loader vive en `src/context/loader.rs`:

- `enum ContextScope { Summary | SummaryFull | Full }`
  (default `Summary`).
- `fn load_from_run_id(home, run_id, scope) -> Result<LoadedContext>`
  — scope `Summary` lee `final/*.md`; `SummaryFull` añade
  `sketches/*.json`; `Full` camina todo el run dir con cap 4 MiB
  por archivo.
- `fn load_from_path(path, scope) -> Result<LoadedContext>`
  — archivo `.md` o directorio caminado recursivamente.
- `fn compute_shared_brief_hash(texts: &[String]) -> String`
  — SHA-256 de la concatenación canónica separada por `\x1f`.
- `fn brief_excerpt(texts, max_chars) -> String` — primer
  `max_chars` caracteres de la concatenación, con `…` al final si
  se trunca.

`LoadedContext { parent_run_id, shared_brief_hash, brief_excerpt, context_refs }`
se pasa al `RunContext` mediante el builder `with_context(...)`. La
fase `IntakePhase` prepende un bloque `[context]...[/context]`
al prompt del LLM y estampa el bloque verbatim sobre
`brief.json#context_block` para que un revisor post-ejecución
pueda reconstruir el input exacto del modelo.

`--context-summary` y `--context-full` son flags de scope; sin
`--context` ambos devuelven `Error::InvalidArgs` para evitar el
foot-gun del no-op silencioso.

### 3.5. Cadenas de referencia

```text
run_disc (parent_run_id = null)
   │
   └── run_deep (parent_run_id = run_disc, shared_brief_hash = sha256(extractions_concat))
            │
            └── run_variant (parent_run_id = run_deep, mode = standard)
```

`run_siblings` se usa para runs lanzados con `moagan rerun --same-config` (mismo parent_run_id, pero el rerun queda como sibling del run original).

---

## 4. Contratos con el LLM

### 4.1. Anatomía de un prompt

```text
[role] <role_id>
[phase] <phase_name>
[brief_hash] <hex>
[constraints_duras]
- <c1>
- <c2>
[constraints_blandas]
- <p1>
[contexto]
<texto>
[instrucciones]
<markdown>
[schema_salida]
```json
<json schema>
```
[salida esperada]
Responde únicamente con un JSON válido. Sin texto fuera del JSON.
```

Los placeholders del prompt se sustituyen en `prompts/registry.rs::render(role, ctx)`. El renderer no toca `\n`; usa `format!` sobre el template embebido.

### 4.2. Catálogo de roles

| role_id | descripción | temperatura default | top_p | max_tokens | json_mode |
|---|---|---:|---:|---:|---|
| `intake` | Normalizar el prompt | 0.0 | 0.2 | 1_000_000 | true |
| `clarify` | Detectar ambigüedades | 0.2 | 0.8 | 1_000_000 | true |
| `router` | Decidir modo | 0.0 | 0.2 | 1_000_000 | true |
| `decomposer` | DAG (sólo deep) | 0.3 | 0.9 | 1_000_000 | true |
| `sketcher` | Sketch corto | 0.7 | 0.95 | 1_000_000 | true |
| `proposer` | Propuesta completa | 0.6 | 0.95 | 1_000_000 | true |
| `critic_correctness` | Crítica de corrección | 0.2 | 0.8 | 1_000_000 | true |
| `critic_constraint` | Crítica de ajuste | 0.2 | 0.8 | 1_000_000 | true |
| `critic_security` | Crítica de seguridad | 0.2 | 0.8 | 1_000_000 | true |
| `judge_correctness` | Juez 1 | 0.0 | 0.2 | 1_000_000 | true |
| `judge_completeness` | Juez 2 | 0.0 | 0.2 | 1_000_000 | true |
| `judge_feasibility` | Juez 3 | 0.0 | 0.2 | 1_000_000 | true |
| `adversary` | Revisión adversaria | 0.3 | 0.9 | 1_000_000 | true |
| `repairer` | Reparación | 0.4 | 0.9 | 1_000_000 | true |
| `tagger` | Tags discovery | 0.0 | 0.2 | 1_000_000 | true |
| `facet_deriver` | Facetas discovery | 0.0 | 0.2 | 1_000_000 | true |
| `extractor` | Extracción por faceta | 0.2 | 0.8 | 1_000_000 | true |
| `integrator` | Borrador integrado | 0.3 | 0.9 | 1_000_000 | true |
| `refiner` | Refinador de fluency | 0.2 | 0.8 | 1_000_000 | true |

### 4.3. Schemas JSON de salida (muestra)

Para `sketcher`:

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": ["thesis","key_decisions","architecture_outline","assumptions","strengths","weaknesses","hard_constraint_check","expected_validation"],
  "properties": {
    "thesis":               { "type": "string", "minLength": 30, "maxLength": 600 },
    "key_decisions":        { "type": "array",  "minItems": 2, "maxItems": 8, "items": { "type": "string", "minLength": 5, "maxLength": 200 } },
    "architecture_outline": { "type": "string", "minLength": 50, "maxLength": 2000 },
    "assumptions":          { "type": "array",  "minItems": 0, "maxItems": 10, "items": { "type": "string" } },
    "strengths":            { "type": "array",  "minItems": 1, "maxItems": 8, "items": { "type": "string" } },
    "weaknesses":           { "type": "array",  "minItems": 1, "maxItems": 8, "items": { "type": "string" } },
    "hard_constraint_check":{ "type": "object", "additionalProperties": { "type": "boolean" } },
    "expected_validation":  { "type": "string", "maxLength": 400 }
  }
}
```

Para `proposer`:

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": ["executive_summary","interpretation","assumptions","goals_and_non_goals","architecture_or_approach","key_flows","implementation_plan","risks","tradeoffs","alternatives_rejected","validation_plan","open_questions","artifacts"],
  "properties": {
    "executive_summary":        { "type": "string", "minLength": 100, "maxLength": 1500 },
    "interpretation":           { "type": "string", "minLength": 50,  "maxLength": 1500 },
    "assumptions":              { "type": "array",  "items": { "type": "string" } },
    "goals_and_non_goals": {
      "type": "object",
      "required": ["goals","non_goals"],
      "properties": {
        "goals":     { "type": "array", "items": { "type": "string" } },
        "non_goals": { "type": "array", "items": { "type": "string" } }
      }
    },
    "architecture_or_approach": { "type": "string", "minLength": 200, "maxLength": 12000 },
    "key_flows":                { "type": "array",  "items": { "type": "string" } },
    "implementation_plan":      { "type": "string", "minLength": 100, "maxLength": 12000 },
    "risks":                    { "type": "array",  "items": { "type": "object", "required": ["description","severity"], "properties": { "description": { "type": "string" }, "severity": { "enum": ["low","medium","high"] } } } },
    "tradeoffs":                { "type": "array",  "items": { "type": "string" } },
    "alternatives_rejected":    { "type": "array",  "items": { "type": "object" } },
    "validation_plan":          { "type": "string", "minLength": 50, "maxLength": 4000 },
    "open_questions":           { "type": "array",  "items": { "type": "string" } },
    "artifacts":                { "type": "array",  "items": { "type": "object", "required": ["kind","path"], "properties": { "kind": { "type": "string" }, "path": { "type": "string" }, "language": { "type": "string" } } } }
  }
}
```

Cada role tiene su schema embebido en código fuente (no en TOML). Esto evita sync entre archivos. Los schemas se almacenan en `src/domain/schemas/<role>.json` y se incluyen con `include_str!`.

### 4.4. Validación de la respuesta

Pipeline de validación JSON en `llm/response.rs`:

1. **Strip código fences**: si la respuesta comienza con ` ```json ... ``` `, extraer el contenido.
2. **Parse JSON**: `serde_json::from_str`.
3. **Validate schema**: `jsonschema::validator_for(schema).validate(&parsed)`.
4. **Si falla**: intento de reparación (ver §4.5).
5. **Si repara**: validar de nuevo.
6. **Si no repara**: `status='error'`, `error_code='JSON_INVALID'`, mensaje en `error_message` (redactado).

### 4.5. Reparación de JSON

Si el schema falla, el sistema hace una única llamada extra al mismo provider/model con `role='json_repair'`:

```text
Tu respuesta anterior no validó contra el schema. Aquí está el schema:
{schema}

Tu respuesta anterior:
{response}

Devuelve únicamente un JSON válido que cumpla el schema.
```

Si la reparación falla, se considera `error` definitivo.

### 4.6. Truncamiento

Si la respuesta termina con un JSON incompleto (sin `}` de cierre), se intenta `serde_json::from_str` con un autocompletado iterativo de `}`/`]` hasta balancear. Si no balancea, se marca `truncated=1` y `status='truncated'`, sin reparación.

### 4.7. Retries

**Tabla de retries** (en `llm/retry.rs`):

| error_code | retries | backoff |
|---|---:|---|
| `HTTP_429` | 3 | 1s, 2s, 4s |
| `HTTP_500` | 2 | 2s, 4s |
| `HTTP_502` | 2 | 2s, 4s |
| `HTTP_503` | 3 | 2s, 4s, 8s |
| `HTTP_504` | 3 | 2s, 4s, 8s |
| `TRANSPORT_ERROR` | 2 | 1s, 2s |
| `JSON_INVALID` | 0 (se repara una vez) | — |
| `TIMEOUT` | 1 | 0s |
| `TRUNCATED` | 1 (aumentando max_tokens al doble) | 0s |
| `SCHEMA_VIOLATION` | 0 (se repara una vez) | — |

Reintentos no se reintentan. El contador `retry_count` se incrementa por intento.

### 4.8. Cancelación cooperativa

Cada llamada LLM recibe un `CancellationToken` (`tokio_util::sync::CancellationToken`). Si el token se dispara mientras el HTTP client está esperando, la llamada retorna `error_code='CANCELLED'`. Si ya se envió la request, se espera la respuesta (no se aborta al provider).

### 4.9. Presupuesto (`Budget`)

```rust
pub struct Budget {
    pub intake:        u64,
    pub decomposition: u64,
    pub sketches:      u64,
    pub full_proposals: u64,
    pub criticism:     u64,
    pub repair:        u64,
    pub validation:    u64,
    pub judging:       u64,
    pub synthesis:     u64,
}
```

Cada fase pide su slot al inicio. Si el `Budget` global está agotado, la fase se omite con `event='error'` y `details_json='{"reason":"budget_exhausted"}'`.

### 4.10. Atribución de modelo

En `manifest.json` y en cada `calls` row:

```json
{
  "model": "minimax",
  "model_version": "MiniMax-M3",
  "provider": "minimax",
  "license": "provider-terms",
  "generated_at": "2026-07-24T10:31:00Z"
}
```

`model_version` se obtiene del `identity` endpoint del provider (cacheado 24h).

---

## 5. Redacción

### 5.1. Punto único de aplicación

`telemetry/redact.rs::apply(input: &str) -> String` aplica todos los patrones configurados. Se invoca:

- Antes de escribir cualquier archivo de telemetría (`calls.jsonl`, `phases.jsonl`, `provider_usage.json`, `run.json`).
- Antes de escribir el `error_message` en SQLite.
- Antes de escribir cualquier log file dentro del run directory.
- Antes de incluir la key en el manifest bajo `api_key_ref` (siempre se redacta).

### 5.2. Patrón (regex)

```text
sk-cp-[A-Za-z0-9]{20,}
sk-[A-Za-z0-9]{20,}
ghp_[A-Za-z0-9]{20,}
gho_[A-Za-z0-9]{20,}
ghs_[A-Za-z0-9]{20,}
ghr_[A-Za-z0-9]{20,}
github_pat_[A-Za-z0-9_]{20,}
xoxb-[A-Za-z0-9-]{20,}
xoxa-[A-Za-z0-9-]{20,}
xoxp-[A-Za-z0-9-]{20,}
AKIA[0-9A-Z]{16}
ASIA[0-9A-Z]{16}
(?i)bearer\s+[A-Za-z0-9._\-+/=]{16,}
(?i)authorization:\s*bearer\s+[A-Za-z0-9._\-+/=]{16,}
(?i)password\s*[:=]\s*\S+
(?i)passwd\s*[:=]\s*\S+
(?i)api[_-]?key\s*[:=]\s*\S+
(?i)secret\s*[:=]\s*\S+
(?i)token\s*[:=]\s*\S+
--token\s+\S+
--api-key\s+\S+
eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}     # JWT
```

### 5.3. Política de redacción

```rust
pub struct RedactPolicy {
    pub in_brief:    bool,  // default false
    pub in_prompts:  bool,  // default false
    pub in_telemetry: bool, // default true
    pub in_storage:   bool, // default true
    pub in_export:    bool, // default true
}
```

El default del binario es `redact_in_telemetry=true` y el resto `false`. El usuario puede cambiar en `[privacy]` del `~/.config/moagan/config.toml`.

### 5.4. Redacción en errores

`llm/response.rs::classify_error` retorna `error_code` y `error_message`. El `error_message` redactado se escribe en `calls.error_message_redacted`; el original en `calls.error_message` (sólo visible si el dashboard fuerza `?raw=1`).

---

## 6. Paralelismo, timeouts y cancelación

### 6.1. `Semaphore` global

```rust
pub struct Parallelism {
    semaphore: Arc<tokio::sync::Semaphore>,
    max: usize,
    in_use: Arc<AtomicUsize>,
}

impl Parallelism {
    pub fn new(max: usize) -> Self;
    pub async fn acquire(&self) -> SemaphorePermit;
    pub fn in_use(&self) -> usize;
    pub fn max(&self) -> usize;
}
```

Adquisición:

```rust
let permit = self.semaphore.acquire().await.unwrap();
self.in_use.fetch_add(1, Ordering::SeqCst);
// ... trabajo ...
self.in_use.fetch_sub(1, Ordering::SeqCst);
drop(permit);
```

### 6.2. Fases que piden paralelismo

Cada fase tiene un campo `desired_parallelism: usize`. Al ejecutar:

```rust
let desired = phase.desired_parallelism();
let free = max_parallelism - parallelism.in_use();
let granted = desired.min(free);
let permits = parallelism.acquire_many(granted as u32).await?;
// ... spawn granted tasks ...
```

### 6.3. Timeouts

Tres niveles, configurables:

```rust
pub struct Timeouts {
    pub sketch: Duration,  // default 120s, 0 = infinite
    pub phase:  Duration,  // default 0 = infinite
    pub total:  Duration,  // default 0 = infinite
}
```

Aplicación:

```rust
let res = tokio::time::timeout(timeout, work).await;
match res {
    Err(_) => Err(Error::Timeout),
    Ok(Err(e)) => Err(e),
    Ok(Ok(v)) => Ok(v),
}
```

Si `timeout == 0`, no se aplica el envoltorio. Esto se traduce en `if timeout != Duration::ZERO { tokio::time::timeout(...) }`.

### 6.4. Cancelación

`tokio_util::sync::CancellationToken`. Una raíz por run. Fases hijas reciben clones. Cancelaciones:

- **Por pausa** (`moagan continue ... --pause`): el token se dispara, las tareas en vuelo completan su iteración actual.
- **Por timeout** (`total`): el token se dispara al expirar.
- **Por shutdown** (SIGINT / SIGTERM): un handler global dispara el token raíz.

### 6.5. Checkpoint humano

`checkpoint/human.rs::ask(ckp: &Checkpoint) -> Result<Resolution>`:

```rust
pub struct Checkpoint {
    pub id: String,
    pub phase: String,
    pub kind: CheckpointKind,
    pub payload: serde_json::Value,
}

pub enum CheckpointKind {
    Intake,
    Clarify,
    Final,
    Custom,
}

pub enum Resolution {
    Approve,
    Modify(serde_json::Value),
    Reject,
    Variant(String), // e.g., "add_constraint"
}
```

No tiene timeout. Usa `dialoguer::Select` o `dialoguer::Input`. El resultado se persiste en `checkpoints/<ckp_id>.json` y se indexa en `checkpoints` SQLite.

#### 6.5.1. Mirror a SQLite vía `Telemetry::record_checkpoint` (Phase D sub-fase #6)

La tabla `checkpoints` definida en `migrations/v001_initial.sql`
sólo tenía las columnas de ciclo de vida (`resolved`, `note`,
`created_unix`, `resolved_unix`) — el `question` y `response`
reales nunca llegaban a SQLite. La sub-fase #6 cierra esa brecha
para que `moagan inspect` pueda responder preguntas como
"¿qué runs tuvieron checkpoints rechazados?" sin parsear cada
`checkpoints/h_<uuid>.json` del filesystem.

**Migración v005** (`src/storage/migrations/v005_checkpoints_content.sql`):

- Añade columnas `ckp_id`, `question`, `response`,
  `accepted_default`, `at_unix`.
- Cambia la PK a `(run_id, ckp_id)` (reconstruye la tabla con
  `CREATE TABLE ... checkpoints_new` + `INSERT INTO ... SELECT`
  + `DROP TABLE` + `RENAME TO`). Los datos legacy se preservan
  con `ckp_id = 'legacy_' || seq`.
- Crea índices `idx_checkpoints_kind` y `idx_checkpoints_at_unix`.
- `PRAGMA user_version` pasa de `4` a `5` (idempotente:
  `if current < 5 { execute V005 }`).

**API de DB** (`src/storage/sqlite.rs`):

```rust
pub fn record_checkpoint(
    &self,
    run_id: RunId,
    ckp_id: &str,
    kind: &str,
    question: &str,
    response: &str,
    accepted_default: bool,
    at_unix: i64,
) -> Result<()>;                                       // INSERT OR REPLACE

pub fn list_checkpoints_for_run(
    &self,
    run_id: RunId,
) -> Result<Vec<CheckpointRow>>;                       // ORDER BY at_unix ASC

pub fn checkpoint_counts_by_kind(
    &self,
    run_id: RunId,
) -> Result<std::collections::BTreeMap<String, i64>>;  // para dashboard
```

**Wiring** (`src/checkpoint/human.rs::persist` + `src/telemetry.rs`):

Cada checkpoint capturado por `ask()` o `skip()` se persiste en dos
artefactos, en este orden:

1. `checkpoints/h_<uuid7>.json` — sidecar canónico (igual que antes).
2. JSONL `telemetry/checkpoints.jsonl` (línea append-only).
3. SQLite `checkpoints` row vía `Telemetry::record_checkpoint`.

El JSON sidecar sigue siendo la fuente de verdad para el audit
trail. La fila SQLite es un **mirror best-effort**: si la DB está
bloqueada o no está abierta, `record_checkpoint` loguea un
`tracing::warn!` y no aborta el run. El mirror es idempotente
(`INSERT OR REPLACE` por `(run_id, ckp_id)`), así que un re-run
del mismo run no duplica filas.

**Disponibilidad**: la migración está activa para cualquier `Db::open`
existente en `v4` — la rama `if current < 5` garantiza que los
runs previos se migran in-place al primer `moagan run` post-merge.

**Queries habilitadas** (estilo `moagan inspect`):

```sql
-- Cuántos checkpoints intake hubo por run
SELECT run_id, COUNT(*) FROM checkpoints WHERE kind = 'intake' GROUP BY run_id;

-- Runs con checkpoints rechazados (el usuario tecleó 'n' o un Modify)
SELECT run_id, COUNT(*) FROM checkpoints WHERE accepted_default = 0 GROUP BY run_id;

-- Último checkpoint por run
SELECT run_id, MAX(at_unix) FROM checkpoints GROUP BY run_id;
```

---

## 7. Sandbox

### 7.1. Estructura

```rust
pub struct Sandbox {
    work_dir: TempDir,
    allowlist: Vec<String>,
    timeout: Duration,
}

pub struct SandboxResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub command: String,
}
```

### 7.2. Aislamiento

- `tempfile::TempDir` con path aleatorio.
- No se expone la red (`std::net` no se bloquea, pero el comando ejecutado en sandbox no debe poder usarla; depende del `command`).
- Variables de entorno saneadas: `PATH` apunta a un dir mínimo, `HOME` apunta al `work_dir`.
- CPU/memoria: no se acota en MVP (limitación documentada en §11).
- Lista de comandos permitidos: `[ "cargo", "rustc", "python", "python3", "tsc", "node", "psql", "sqlite3", "jq", "cat", "ls", "find", "grep" ]`.

### 7.3. Ejecución

```rust
impl Sandbox {
    pub async fn run(&self, cmd: &str, args: &[&str]) -> Result<SandboxResult> {
        let mut command = tokio::process::Command::new(cmd);
        command
            .args(args)
            .current_dir(&self.work_dir)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", self.work_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let start = Instant::now();
        let output = tokio::time::timeout(self.timeout, command.output()).await??;
        Ok(SandboxResult {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            duration: start.elapsed(),
            command: format!("{} {}", cmd, args.join(" ")),
        })
    }
}
```

### 7.4. Interpretación

- `exit_code == 0`: `Pass`.
- `exit_code != 0`: `Fail`.
- Comando no disponible: `Skipped` (registrado en `validation/skipped_checks`).
- Test ausente: `Skipped` (no se asume `Pass`).

---

## 8. Pipeline de modos no-discovery

### 8.1. Fases (ordenadas)

```
0. intake
1. clarify
2. route
3. decompose          (sólo deep)
4. sketch_phase
5. proposal
6. gate
7. validate           (si aplica)
8. critique
9. repair             (0..2 rondas)
10. judge
11. rank
12. cluster_proposals
13. synthesize         (opcional, si hay compatibilidad)
14. checkpoint_final   (interactivo)
15. deliver
```

### 8.2. Estado de una propuesta

```text
Draft
  ↓
StructurallyValid           (gate)
  ↓
MechanicallyValidated       (validate)
  ↓
Critiqued                   (critique)
  ↓
Revised                     (repair, 0..n)
  ↓
Evaluated                   (judge)
  ↓
Ranked                      (rank)
  ↓
Selected | Alternative | Rejected
```

Persistencia:

- Cada transición escribe un sidecar (e.g., `proposals/p_<id>.json` para `Draft`, `critiques/p_<id>_critic_<role>.json` para `Critiqued`, `revisions/p_<id>_rev_<n>.json` para `Revised`).
- En SQLite, la tabla `phases` registra eventos de las fases, no de las propuestas.

### 8.3. Selección (Fase 11)

#### Paso 1: filtros duros

```text
if proposal.hard_constraint_violations.any():
    eliminar
if proposal.blockers.any():
    eliminar
if proposal.required_validation == 'required' and validation.status == 'Fail':
    eliminar
if proposal.correctness < MIN_CORRECTNESS:
    eliminar
```

#### Paso 2: vector de calidad

```rust
pub struct QualityVector {
    pub correctness:     f32,
    pub completeness:    f32,
    pub feasibility:     f32,
    pub alignment:       f32,
    pub maintainability: f32,
    pub security:        f32,
    pub cost:            f32,
    pub clarity:         f32,
    pub novelty:         f32,
}
```

#### Paso 3: frente de Pareto

`ranking/pareto.rs::pareto_front(proposals: &[Proposal]) -> Vec<usize>`:

```rust
fn dominates(a: &QualityVector, b: &QualityVector) -> bool {
    let mut any_better = false;
    for (x, y) in [a,b].iter().zip([a,b].iter()) {} // pseudocódigo
    // estrictamente >= en todas las componentes y estrictamente > en al menos una
}
```

Sólo se conservan las no-dominadas.

#### Paso 4: diversidad

Si el frente tiene > `top_k`:

```rust
let clusters = cluster_by_stack(&front);
let representatives = pick_with_crowding_distance(clusters, top_k);
```

#### Paso 5: ranking ponderado

```rust
fn weighted(weights: &Weights, v: &QualityVector) -> f32 {
    weights.iter().zip(v.iter()).map(|(w, x)| w * x).sum()
}
```

`Weights` se perturbó en `±0.05` en `stability.rs` para test:

```rust
fn stability_score(proposals: &[Proposal], weights: &Weights, n_perturbations: usize) -> HashMap<Uuid, f32> {
    // cuenta cuántas perturbaciones mantienen a esta propuesta en el top-1
}
```

### 8.4. Síntesis (Fase 13)

1. Detectar clusters compatibles.
2. Para cada cluster, generar una propuesta `SynthesisProposal` con:

```json
{
  "id": "syn_<uuid7>",
  "source_proposals": ["p_aaa", "p_bbb"],
  "cluster_id": "cluster_2",
  "synthesis_strategy": "merge_invariants"
}
```

3. Correr la propuesta `syn` por las mismas fases (gate → validate → critique → repair → judge → rank).
4. Sólo se presenta al usuario si supera a las fuentes.

#### 8.4.1. Propagación de la síntesis al pipeline

`SynthesizePhase` escribe el resultado en dos lugares (`src/phases/synthesize.rs`):

- `synthesized/s_<NN>.json` — registro de línea genealógica inmutable
  que conserva `source_proposals`, `cluster_id`, `synthesis_strategy`,
  y los `tradeoffs`/`evidence` agregados. Es el artefacto auditable.
- `proposals/s_<NN>.json` — copia con shape `Proposal` (campos
  `id`, `summary`, `approach`, `tradeoffs`, `evidence`,
  `source_sketch`, `artifacts`) para que las fases `Gate → Critique
  → Repair → Judge → Rank → Deliver` la procesen como una propuesta
  más y entre al frente de Pareto (cumple §5.13 "La síntesis
  compite").

El prefijo `s_` evita colisión con `p_<NN>` en `proposals/` y
permite a `DeliverPhase` marcar la insignia **"synthesis"** en el
portfolio (ver `kind_badge_for` en `src/phases/deliver.rs`). El
campo `source_sketch` de la copia copiada se rellena con
`syn_from_<cluster_id>` para que las fases siguientes puedan
reconstruir la línea genealógica si lo necesitan.

Coste LLM adicional por run (siempre-on en `standard`, `deep`,
`batch`; omitido en `fast`):

| Modo | Críticas extra | Jueces extra | Adversario extra |
|---|---|---|---|
| `standard` | 2-3 | 5 | 0-1 |
| `deep` | 3-4 | 7 | 0-1 |
| `batch` | 2-3 | 5 | 0-1 |
| `fast` | 0 | 0 | 0 (síntesis omitida) |

Este coste es intencional y refleja la regla de §5.13: la síntesis
sólo sustituye a sus fuentes si demuestra mejora, lo que requiere
ser evaluada en igualdad de condiciones.

#### 8.4.2. Reemplazo de fuentes (Phase F)

V4 §5.13 dice: *"Solo sustituye a sus fuentes si demuestra mejora
sin perder coherencia."* Esta sub-fase aterriza la semántica.

**Predicado** (de `proposal-03-add-ons.md` D.13.16, catálogo aditivo,
adaptado a dimension-counting):

```rust
pub fn should_replace_synthesis(
    synthesis_v: &QualityVector,
    source_vs: &[QualityVector],
) -> bool {
    // 1. La síntesis es la mejor estricta (entre todas las fuentes)
    //    en ≥2 de las 5 dimensiones de calidad.
    // 2. Ninguna fuente Pareto-domina a la síntesis (reusa
    //    `ranking::pareto::dominates`).
}
```

Implementado en `src/phases/replace.rs`. Reutiliza
`crate::ranking::pareto::dominates` para el bloqueo Pareto. El
umbral de 2 dimensiones es el suelo de "mejora no trivial" que
V4 §5.13 llama explícitamente. La adaptación a dimension-counting
(con respecto al source-counting de D.13.16) se discutió en la
sesión del 2026-07-30: permite que clusters de un solo miembro
sean reemplazados cuando la síntesis gana claramente.

**Comportamiento por defecto**:

| Modo | Reemplaza fuentes |
|---|---|
| `fast` | OFF (no sintetiza) |
| `standard` | ON |
| `deep` | ON |
| `batch` | ON |
| `continue` | ON (re-corre `RankPhase` sobre evaluaciones existentes) |

**Opt-out**: `--no-replace-sources` en `moagan run`. El flag
sobreescribe el default por modo.

**Efectos colaterales**:

1. `proposals/p_<id>.json` gana `replaced_by: "s_<NN>"` cuando su
   fuente fue reemplazada. El campo es `#[serde(default, skip_if_none)]`
   para mantener compatibilidad con sidecars existentes.
2. `rankings/ranking.json::ranked` se filtra para excluir las
   fuentes reemplazadas.
3. `rankings/ranking.json::representatives` gana la síntesis si no
   estaba ya seleccionada por el paso de crowding.
4. `synthesized/s_<NN>.json` mantiene `source_proposals` intacto —
   la línea genealógica se conserva.

**Origen de la línea genealógica**: `RankPhase` lee
`synthesized/s_<NN>.json` (el sidecar inmutable escrito por
`SynthesizePhase`) para resolver el mapeo `s_<NN>` →
`source_proposals[]`. Es la única fuente de verdad para esa
relación (sesión 2026-07-30): el id `s_00` no se corresponde
necesariamente con `cp_00` porque clusters saltados (singletons)
desplazan el índice.

**Limitación conocida**: `proposal-03-add-ons.md` D.13.13 define
`HARD_INCOMPATIBILITIES` (pares como `("monolith", "microservices")`
donde la síntesis debe bloquearse). Esta sub-fase NO implementa la
verificación de incompatibilidades — queda como opt-in del catálogo
aditivo para una sub-fase futura.

**Coste LLM adicional**: 0. La fase `RankPhase` ya corre sobre
evaluaciones existentes; sólo cambia qué ids se mantienen en el
ranking final.

#### 8.4.3. Estabilidad (Phase H)

V4 §5.12 paso 6: *"Se perturban los pesos dentro de un rango
pequeño. Si top-1 sigue ganando, el ranking es estable. Si cambia,
se marca como sensible a preferencias."* Esta sub-fase aterriza la
semántica y, de paso, dispara el segundo trigger del checkpoint
humano de V4 §5.14 («el ranking es inestable»).

**Perturbación** (`src/ranking/stability.rs`):

```rust
pub fn perturb_weights(
    base: &RankingWeights,
    n: usize,
    sigma: f32,
    seed: u64,
) -> Vec<RankingWeights>;

pub fn stability_score(
    weights_set: &[RankingWeights],
    evaluations: &[(String, EvalSnapshot)],
) -> HashMap<String, f32>;
```

- Ruido gaussiano Box-Muller (vía `fastrand`, ya en deps) añadido
  a cada uno de los seis pesos. Sin nuevas dependencias.
- Clip a `[0.0, 2.0]` por peso: sin pesos negativos (romperían
  el contrato de promedio ponderado) y sin valores que dominen al
  base weight más allá de lo razonable.
- `n = 0` o `sigma <= 0` cortocircuita a `vec![]` (no-op).

**Score** (`src/ranking/stability.rs::stability_score`):

Para cada `weights` del set se recalcula el ranking; el argmax
incrementa un contador de victorias. El resultado es un
`HashMap<proposal_id, f32>` con la fracción `[0.0, 1.0]` de
perturbaciones bajo las que el proposal mantuvo su posición.
Tiebreak determinista por `id` ascendente — los `HashMap`
resultantes son estables entre runs.

**Etiqueta** (`StabilityLabel`):

```rust
pub enum StabilityLabel { Stable, Sensitive }
```

`score >= sensitive_threshold` ⇒ `Stable`, en otro caso
`Sensitive`. El threshold vive en
`Config::stability.sensitive_threshold` (default `0.8`).

**Sidecar**:

Tres campos nuevos en `Ranking` (todos
`#[serde(default, skip_serializing_if = "Option::is_none")]` para
compatibilidad con sidecars v0.2):

- `stability_score: Option<HashMap<String, f32>>`
- `stability_label: Option<StabilityLabel>`
- `stability_sigma: Option<f32>` (sigma usado en la perturbación
  efectiva, registrado para correlación con la sensibilidad
  detectada)

`None` significa "el check se omitió" — `Config::stability.enabled
== false`, `n_perturbations == 0`, o el run tiene una sola
propuesta (trivialmente estable).

**Wiring en `RankPhase`**: paso 5.6, insertado entre el paso 5.5
(reemplazo de fuentes, Phase F) y el write final de
`rankings/ranking.json`. Sobre el vector `items: Vec<(String,
Aggregated, String)>` ya cargado en el paso 1, construye snapshots
`EvalSnapshot` por proposal y los pasa a `stability_check`.

**Sigma por modo**:

| Modo | Sigma | Razonamiento |
|---|---|---|
| No interactivo (`--non-interactive`, `Mode::Batch`) | `sigma_default` (0.05) | Conservador; perturbaciones pequeñas no voltean ganadores claros |
| Interactivo (`standard`, `deep`) | `sigma_interactive` (0.10) | Mayor sensibilidad para detectar preferencias inestables antes del checkpoint |

El veredicto se calcula sobre el ranking **post-reemplazo** (paso
5.5 ya ejecutado). Es la semántica correcta: queremos saber si el
**ranking final** es estable, no el pre-reemplazo.

**Disparo del checkpoint** (V4 §5.14 segundo trigger):

Cuando `stability_label == Some(StabilityLabel::Sensitive)` y
`ctx.interactive == true`, `RankPhase` invoca
`checkpoint::ask` con `CheckpointKind::Custom` y la pregunta:

> *"Ranking is sensitive to weight perturbation (top-1 stability
> {top_score:.2}, threshold {threshold:.2}, sigma {sigma:.2}).
> Continue with the current winner '{winner}'?"*

Default = "yes" (continuar). El usuario puede aceptar, rechazar
(`n`), o free-form (`Modify`) — la respuesta se persiste en
`checkpoints/h_<uuid>.json` y se mirror-ea a SQLite vía
`Telemetry::record_checkpoint`.

Runs no interactivos (`--non-interactive`, `Mode::Batch`) caen en
`checkpoint::skip` que escribe el marker
`<skipped:non_interactive>` para auditoría. El trigger está
**disparado** (el veredicto es Sensitive) pero el prompt se
suprime.

**Propagate to `Proposal.source_nodes`** (Phase G limitation #2
follow-up): además de la estabilidad, esta sub-fase cierra el
commit #2 del follow-up. `ProposePhase::compute_source_nodes`
lee `problem_graph.json` cuando es no-trivial y asigna a cada
proposal los ids de los nodos cuyo texto pasa el umbral Jaccard
`SOURCE_NODE_THRESHOLD = 0.7` (mismo umbral que el
`CLUSTER_THRESHOLD` de `RankPhase`). La asignación se ordena por
`(distancia, id)` para que la escritura sea idempotente.

**Limitaciones documentadas**:

1. `proptest` no se añade como dep — los invariantes de la
   perturbación (clip, monotonicidad de sigma, fracciones suman
   1.0) están cubiertos por tests unitarios con seeds fijos.
2. El mirror a SQLite del verdict de estabilidad se hace vía
   `tracing::info!` por ahora; un nuevo campo en la tabla `runs`
   puede aterrizar en sub-fase posterior (Phase I, dashboard).
3. `Resolution::Modify(text)` del checkpoint es informativo; el
   pipeline no re-rankea con la nota del usuario todavía — un
   follow-up puede cablearlo a `moagan rerank`.

**Coste LLM adicional**: 0. El check es pura cómputo sobre
evaluaciones existentes.

**Configuración** (`Config::stability`):

```toml
[stability]
enabled              = true     # default
n_perturbations      = 8        # default
sigma_default        = 0.05     # default
sigma_interactive    = 0.10     # default
sensitive_threshold  = 0.8      # default
seed                 = 0xDEFA17_BEEF  # default
```

### 8.5. Checkpoint final

Sólo si:

- `Mode ∈ {fast, standard, deep, explore, batch}` y `interactive=true`.
- `mode == 'auto'` es lo opuesto; se entrega directamente.

Acciones:

```text
[S] Seleccionar P<n>
[A] Seleccionar familia
[W] Cambiar pesos
[C] Añadir restricción
[V] Pedir variante
[Y] Síntesis dentro de cluster
[R] Profundizar riesgo
[T] Terminar sin elegir
```

---

## 9. Pipeline de discovery

### 9.1. Matriz de exploración

```rust
pub struct ExplorationMatrix {
    pub roles: Vec<String>,        // ej. ["sketcher", "sketcher_pragmatic", "sketcher_sec"]
    pub models: Vec<ModelSpec>,    // ej. [{provider:"minimax",model:"MiniMax-M3"},{provider:"glm",model:"glm-4.6"}]
    pub temperatures: Vec<f32>,    // ej. [0.3, 0.6, 0.9]
}

impl ExplorationMatrix {
    pub fn cardinality(&self) -> usize {
        self.roles.len() * self.models.len() * self.temperatures.len()
    }
}
```

### 9.2. Generación

```rust
let sem = Semaphore::new(max_parallelism);
let tasks = matrix.iter().map(|cell| {
    let sem = sem.clone();
    let cell = cell.clone();
    async move {
        let permit = sem.acquire().await.unwrap();
        let sk = generate_sketch(cell, brief).await?;
        drop(permit);
        Ok(sk)
    }
});
let sketches = futures::future::join_all(tasks).await;
```

### 9.3. Política de paro

```rust
pub struct StopPolicy {
    pub saturation_threshold: f32,  // default 0.7
    pub margin_frac: f32,           // default 0.25
    pub outlier_distance: u32,      // default 32 (SimHash bits)
}
```

Loop:

```rust
async fn discovery_loop(matrix: Matrix, brief: Brief, stop: StopPolicy) -> Vec<Sketch> {
    let mut next_models = matrix.models.clone();
    let mut sketches = Vec::new();
    let mut total_generated = 0;
    let mut stopped_per_model = HashMap::new();

    loop {
        let batch = generate_batch(&next_models, 40).await;
        let clusters = cluster_simhash(&batch);
        let aporte = coverage_new(&clusters, &sketches);
        let outliers = detect_outliers(&batch, &clusters, stop.outlier_distance);

        sketches.extend(batch);
        sketches.extend(outliers);     // outliers siempre se preservan
        total_generated += batch.len() + outliers.len();

        if aporte < stop.saturation_threshold {
            let saturated = all_models_saturated(&batch, &next_models);
            if saturated {
                let queue = (total_generated as f32 * stop.margin_frac).ceil() as usize;
                let extra = generate_batch(&next_models, queue).await;
                sketches.extend(extra);
                break;
            } else {
                next_models = unsaturated_models(&next_models, &batch);
            }
        } else {
            // siguiente iteración normal
        }

        if total_generated >= matrix.cardinality() {
            break;
        }
    }
    sketches
}
```

### 9.4. Tagger

```rust
pub struct Tags {
    pub primary: String,
    pub secondary: Vec<String>,
    pub similarity_to_category: Option<f32>,
}
```

Si `similarity_to_category < 0.6` para todas las categorías → `primary = "uncategorized"`.

Modo de cálculo de similarity:

- Para comparativa con categorías existentes, se usa SimHash.
- Si `uncategorized` moda > `uncategorized_threshold` (default 0.3), warning en `manifest.json`.

### 9.5. Clustering

```rust
pub fn cluster_simhash(sketches: &[Sketch]) -> Vec<Cluster> {
    // 1. Calcular simhash de cada sketch:
    //    - Tokenizar (whitespace + lowercase + filtro stopwords)
    //    - Para cada token, hash con FNV-64 a 64 bits
    //    - Sumar +/- 1 por bit
    //    - Bit final del simhash
    //
    // 2. Union-find con threshold 0.85 (distancia Manhattan <= 9)
    // 3. Asignar cluster_id incremental
    // 4. Calcular centroid = moda de simhashes del cluster
}
```

### 9.6. Detector de contradicciones

```rust
pub fn detect_contradictions(clusters: &[Cluster], sketches: &[Sketch]) -> Vec<Contradiction> {
    // Pares de clusters con distancia_simhash > 30
    // Por cada par, llamada LLM con role='contradiccion':
    //   Input: 2 sketches representativos
    //   Output: {topic, description, severity}
}
```

### 9.7. Facetas

```rust
pub struct Facet {
    pub id: String,             // "flujos"
    pub description: String,
    pub required: bool,
}
```

Cache por `sha256(brief.json || category_id)`. Archivo: `facets/cat_<id>_facets.json`.

### 9.8. Extracción por faceta

```rust
pub async fn extract_facet(category: &Cluster, facet: &Facet, briefs: &[Brief]) -> Result<Markdown> {
    // role = 'extractor'
    // input: cluster briefs + contradictions + facet
    // output: markdown
}
```

Paralelismo: `parallelism.extraction`.

### 9.9. Integración híbrida

```rust
pub async fn integrate(extractions: HashMap<String, Markdown>, brief: &Brief) -> Result<Document> {
    // 1. Script unir: concatena en orden de facetas required-first
    let draft = script::join(extractions)?;

    // 2. LLM validador (role='integrator'):
    let issues = llm_integrator_validate(&draft).await?;

    // 3. Si issues no-vacío:
    //    - Prompts al usuario (opcional)
    //    - O fusión automática de contradicciones
    if !issues.is_empty() {
        let draft = fuse_contradictions(draft, issues)?;
    }

    // 4. Refinador (role='refiner')
    let final_doc = llm_refiner(&draft).await?;

    Ok(final_doc)
}
```

Reglas:

- Refinador no debe eliminar contenido. Si la longitud baja > 20%, se revierte.
- Si la coherencia inter-documento cae, se aborta y se avisa al usuario.

### 9.10. Documento `uncategorized`

```rust
if uncategorized_count >= 3 {
    write_uncategorized_md(&sketches)?;
}
```

Si `< 3`, sólo se registra en `manifest.json`.

### 9.11. Checkpoint humano único

Sólo en modo interactivo. Acciones disponibles:

```text
[approve] Aprobar todos
[review]  Revisar documento <cat_id>
[block]   Bloquear documento <cat_id>
[export]  Exportar
```

No permite crear/renombrar categorías.

---

## 10. Continuidad y operación

### 10.1. `moagan run`

```rust
pub async fn run(cli: RunArgs) -> Result<()> {
    let root = root_dir()?;
    let run_id = new_run_id();
    let root = root.join(".runs").join(run_id.to_string());
    fs::create_dir_all(&root)?;

    // 1. manifest.json inicial
    let manifest = Manifest::init(&cli, run_id, &root)?;
    fs::write(root.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;

    // 2. SQLite
    let db = Db::open(&root_dir()?)?;
    db.insert_run(&manifest)?;

    // 3. Cancel token
    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());

    // 4. Pipeline
    let pipeline = Pipeline::new(manifest, db, cancel);
    pipeline.run().await
}
```

### 10.2. `moagan continue`

```rust
pub async fn continue_run(cli: ContinueArgs) -> Result<()> {
    let run_id = cli.run_id.unwrap_or_else(|| last_active_run()?);
    let manifest = Manifest::load(run_id)?;
    let db = Db::open(&root_dir()?)?;

    // 1. Si --switch-provider, actualizar manifest
    if let Some(p) = cli.switch_provider {
        manifest.switch_provider(p, "user")?;
        db.record_provider_change(&manifest)?;
    }

    // 2. Si --switch-api-key, actualizar
    if let Some(k) = cli.switch_api_key {
        let key = resolve_api_key(k)?;  // interactive | env: | file:
        manifest.api_key_redacted = redact(&key);
        db.update_api_key_ref(&manifest, &key)?;
    }

    // 3. Buscar última fase completada
    let last_phase = last_completed_phase(&db, run_id)?;
    let pipeline = Pipeline::resume(manifest, db, last_phase);
    pipeline.run().await
}
```

#### 10.2.1. Implementación (v0.3 sub-fase J)

`moagan continue` reemplaza el stub v0.2 con una implementación
real:

1. Resolver `MOAGAN_HOME` y abrir el SQLite (`Db::open`).
2. Cargar el `manifest.json` del run vía
   `continue_cmd::load_manifest`.
3. Si `--switch-provider <name>` está presente, estampar el
   cambio sobre `manifest.provider` y registrar una fila en
   `provider_changes` con la razón `'user --switch-provider'`.
4. Si `--switch-api-key <spec>` está presente, resolver via
   `resolve_api_key_spec` (acepta `env:VAR`, `file:path`, o
   literal; rechaza `prompt:` por la no-go list de AGENTS). El
   valor resuelto se redacta para stderr (`head***tail`) y se
   logea el prefijo SHA-256 de 8 chars para auditoría.
5. Si `--skip-checkpoint` está presente, registrar un evento
   sintético `checkpoint:skipped` en `provider_changes`.
6. `Db::last_completed_phase(run_id)` devuelve la última fase
   con `status='end'`. Si no hay ninguna, `Error::InvalidState`.
7. `Pipeline::resume(canonical, last_phase)` filtra el pipeline
   canónico para saltarse las fases cuyo índice canónico sea
   `<= last_phase`.
8. Re-correr el pipeline filtrado.

El resultado se persiste en `manifest.json` antes de empezar el
pipeline (sidecar atómico via `AtomicWriter`).

### 10.3. `moagan resume`

Igual que `continue` pero sin flags de switch. Asume estado consistente.

#### 10.3.1. Implementación (v0.3 sub-fase J)

`moagan resume <run_id>` se implementa como `run_continue(run_id,
ContinueOptions::default())` — el mismo camino pero sin los
flags de switch. Esto evita duplicación: cualquier futura mejora
a `continue` se aplica automáticamente a `resume`.

### 10.4. `moagan rerun`

```rust
pub async fn rerun(cli: RerunArgs) -> Result<()> {
    let old = Manifest::load(cli.run_id)?;
    let new_id = new_run_id();
    let mut new = old.clone();
    new.run_id = new_id;
    new.parent_run_id = Some(old.run_id);
    new.created_at = now();
    new.status = "created";

    fs::create_dir_all(root_dir()?.join(".runs").join(new_id.to_string()))?;
    fs::write(root.join("manifest.json"), serde_json::to_string_pretty(&new)?)?;

    let db = Db::open(&root_dir()?)?;
    db.insert_run(&new)?;
    db.add_sibling(old.run_id, new.run_id)?;

    if let Some(overrides) = cli.matrix_override {
        apply_overrides(&mut new, overrides)?;
    }

    let pipeline = Pipeline::new(new, db, CancellationToken::new());
    pipeline.run().await
}
```

#### 10.4.1. Implementación (v0.3 sub-fase J)

`moagan rerun <run_id> [--matrix-override <json>] [--same-config]`:

1. Cargar el manifest del run origen vía
   `continue_cmd::load_manifest`.
2. `clone_manifest_for_rerun` clona el manifest con un
   `run_id` fresco (UUID v7), `parent_run_id = old.run_id`,
   `status = "created"`, `phases = []`, `usage = default()`.
3. Si `--matrix-override <json>` está presente, se aplica
   `merge_value` (deep-merge recursivo de `serde_json::Value`)
   sobre un target sintético que contiene `brief.problem` y otros
   placeholders. Esto deja la puerta abierta para un bloque
   `execution_policy` futuro sin romper el contrato actual.
4. `write_manifest_to_disk` persiste el manifest atómicamente.
5. `db.register_run` inserta la fila nueva en `runs` con el
   `shared_brief_hash` y `parent_run_id` heredados.
6. `db.add_run_sibling_relation(old, new, "rerun")` enlaza los
   dos runs via `run_siblings` con `relation = 'rerun'`.
7. `db.update_run_status(new, "running")` voltea el status.
8. `resume_pipeline(&home, &new, "intake")` arranca el pipeline
   desde el principio (rerun no resume; re-ejecuta).

`--override-json` y `--matrix-override` son alias; si ambos
están presentes, `--matrix-override` gana (es el nombre bendecido
por el spec).

### 10.5. `moagan inspect`

```rust
pub async fn inspect(cli: InspectArgs) -> Result<()> {
    let db = Db::open(&root_dir()?)?;
    let run = db.get_run(cli.run_id)?;
    let phases = db.get_phases(cli.run_id)?;
    let calls = db.get_calls(cli.run_id)?;

    println!("Run: {} ({})", run.run_id, run.mode);
    println!("Status: {}", run.status);
    println!("\nPhases:");
    for p in phases {
        println!("  - {} {} at {}", p.phase_name, p.event, p.at);
    }

    if let Some(phase_name) = cli.phase {
        let phase_calls: Vec<_> = calls.iter().filter(|c| c.phase == phase_name).collect();
        println!("\nCalls in {}: {}", phase_name, phase_calls.len());
    }

    Ok(())
}
```

### 10.6. `moagan import`

```rust
pub async fn import(cli: ImportArgs) -> Result<()> {
    let src = PathBuf::from(cli.source_path);
    let manifest_path = src.join("manifest.json");
    let manifest: Manifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;

    let dest = root_dir()?.join(".runs").join(&manifest.run_id);
    if dest.exists() {
        return Err(Error::AlreadyExists(manifest.run_id));
    }

    fs::create_dir_all(&dest)?;
    copy_dir_all(&src, &dest)?;

    let db = Db::open(&root_dir()?)?;
    db.upsert_run(&manifest)?;

    println!("Imported run {}", manifest.run_id);
    Ok(())
}
```

#### 10.6.1. Implementación (v0.3 sub-fase J)

`moagan import --source-path <dir> [--target-runs-dir <dir>]`:

1. Validar que `<source>/manifest.json` exista. Si no,
   `Error::InvalidArgs("source manifest not found at ...")`.
2. Parsear el manifest vía `serde_json::from_slice`. Esto valida
   el `run_id` y el resto del contrato.
3. Resolver el destino: `--target-runs-dir` si se da, si no
   `<MOAGAN_HOME>/.runs`.
4. Si el destino ya existe, `Error::InvalidState` (no se
   sobreescribe — el operador debe hacer `moagan rerun` o borrar
   primero).
5. `move_dir` mueve el directorio: `fs::rename` en el mismo
   filesystem, fallback a `copy_dir_recursive + remove_dir_all`
   cuando `EXDEV` (cross-device).
6. `db.register_run` reinserta la fila con `parent_run_id`,
   `shared_brief_hash`, y demás metadata preservada.
7. `db.add_context_ref` re-mirror cada `ContextRefRecord` del
   manifest en la tabla `run_context_refs`.

Resultado: el manifest queda en `<MOAGAN_HOME>/.runs/<id>/` y
el SQLite index contiene la fila para `moagan inspect`.

### 10.7. `moagan telemetry`

Subcomandos:

```rust
pub enum TelemetryCmd {
    List { run: Option<Uuid> },
    Summary { run: Uuid },
    Compare { run_a: Uuid, run_b: Uuid },
    Provider { plan: Option<String>, list: bool },
    View { port: u16 },
    Export { run: Uuid, format: ExportFormat, level: ExportLevel },
    Cleanup { dry_run: bool },
    Config,
    Verify { path: PathBuf },
}
```

#### 10.7.1. Implementación (v0.3 sub-fase I)

Cada variante de `TelemetryCmd` se implementa en un submódulo
de `src/cli/telemetry_cmd.rs` y se despacha vía
`TelemetryCmd::dispatch(self) -> Result<i32>`. La función
`resolve_home(runs_dir: Option<&Path>) -> Result<MoaganHome>`
factoriza la resolución del home (override `--runs-dir` o
`MOAGAN_HOME`).

**Subcommand list (`--limit <N> [--run <id>]`)**:
- Sin `--run`: tabla compacta de los N runs más recientes
  ordenados por `created_unix DESC`. Cada fila lleva short
  id, mode, status, call count y total tokens.
- Con `--run`: drill-in de un solo run con `RunRow` +
  `RunAggregate` + phases + provider_usage.

**Subcommand summary (`--run <id>`)**:
Imprime duración wall-clock + counters + secciones
`by model` / `by phase`. Lee `updated_unix` desde SQLite
cuando está disponible; cae a mtime del filesystem.

**Subcommand compare (`--run-a <A> --run-b <B>`)**:
Side-by-side + delta por métrica.

**Subcommand provider (`--list` o `--plan <name>`)**:
`--list` imprime el roster; `--plan` drilea en un provider
configurado + recent usage.

**Subcommand view (`--port <port>`)**:
Servidor HTTP read-only (ver §10.8).

**Subcommand export (`--run <id> [--level] [--format] [--out]`)**:
Bundle del run (ver §10.9).

**Subcommand cleanup (`--dry-run`)**:
Retention pass (ver §10.10).

**Subcommand verify (`--path <archive|dir>`)**:
Re-hash del bundle (ver §10.10).

### 10.8. Dashboard

`axum` se evita para no añadir dep. Se usa un servidor minimalista hecho con `tokio::net::TcpListener` + `hyper` sólo si lo necesitamos. Decisión: **no usar `hyper` ni `axum`**. En su lugar, un servidor minimalista sobre `tokio` parseando HTTP manualmente. Esto reduce deps.

Alternativa más simple: **`tiny_http` o un mini servidor `tokio` propio**. Voy con `tokio` propio para cero deps adicionales.

```rust
// src/telemetry/dashboard.rs
pub async fn serve(port: u16, db: Db) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let db = db.clone();
        tokio::spawn(async move {
            handle_request(stream, db).await;
        });
    }
}
```

Endpoints:

```text
GET /api/runs
GET /api/runs/{run_id}
GET /api/runs/{run_id}/phases
GET /api/runs/{run_id}/calls
GET /api/runs/{run_id}/provider_usage
GET /api/runs/{run_id}/hashes
GET /api/runs/{run_id}/export?level=summary&format=tar.gz
```

Body: JSON. Sin estado mutable. Sin autenticación (sólo 127.0.0.1).

#### 10.8.1. Implementación (v0.3 sub-fase I)

El servidor se implementa en `src/telemetry/dashboard.rs`.
`axum`, `hyper` y `tiny_http` siguen excluidos (no-go list +
decisión §10.8). El parser HTTP/1.1 está hecho a mano sobre
`tokio::io::BufReader` + `AsyncReadExt` / `AsyncWriteExt`.

Estructura:

```rust
pub struct DashboardConfig {
    pub bind: SocketAddr,
    pub home: Arc<MoaganHome>,
    pub db_path: Option<PathBuf>,
}

pub struct DashboardHandle {
    pub local_addr: SocketAddr,
    /* cancellation token + JoinHandle (private) */
}

pub async fn start(cfg: DashboardConfig) -> Result<DashboardHandle> { /* ... */ }
```

Constantes expuestas:
- `DEFAULT_PORT = 4096` (V4 §8.8).
- `PORT_BLACKLIST: &[u16] = &[22, 80, 443, 3306, 5432, 6379,
  8080, 8443]`. La función libre `pick_port(requested)`
  evita esos puertos y avanza hasta 1000 hacia adelante
  hasta encontrar uno libre.
- `MAX_HEADER_BYTES = 8 * 1024`.
- `IO_TIMEOUT = 30s` por conexión.

Comportamiento:
- Loopback only: `cfg.bind.ip().is_loopback()` se valida
  antes del `bind`. Direcciones no-loopback devuelven
  `Error::InvalidArgs`.
- `tokio::spawn` por conexión. Cada handler corre bajo
  `tokio::time::timeout(IO_TIMEOUT, …)` para abortar
  conexiones colgadas.
- `Connection: close` en cada respuesta para evitar
  bookkeeping de keep-alive.

Dispatch (`fn dispatch(path, query, cfg) -> Result<Response,
(u16, String)>`):

| Path                                  | Backend                                    |
|---------------------------------------|--------------------------------------------|
| `/`                                   | Landing page (texto plano con la lista de endpoints). |
| `/api/runs?limit=N`                   | `db.list_runs(limit)`, serializado como JSON. |
| `/api/runs/{id}`                      | `{ run, aggregate, phases, provider_usage }`. |
| `/api/runs/{id}/phases`               | `db.list_phase_summaries_for_run(id)`. |
| `/api/runs/{id}/calls`                | `db.list_calls_for_run(id)`. |
| `/api/runs/{id}/provider_usage`       | `db.list_provider_usage_for_run(id)`. |
| `/api/runs/{id}/hashes`               | `compute_hashes(run_dir.root())` (walkdir + `sha2`). |
| `/api/runs/{id}/export?level=…&format=…` | Llama a `telemetry::export::export_run` y devuelve el JSON resumen. |
| OTRO                                   | `404 Not Found`. |

Códigos de error:
- `400 Bad Request` para run_id inválido, query mal
  formada, export level/format desconocido.
- `404 Not Found` para run inexistente o sub-recurso
  desconocido.
- `500 Internal Server Error` sólo para fallos del
  backend (SQLite corrupto, etc.).
- `405 Method Not Allowed` para todo lo que no sea GET
  (`POST`/`PUT`/`DELETE`).

### 10.9. Export

```rust
pub fn export_run(run_id: Uuid, format: ExportFormat, level: ExportLevel, dest: PathBuf) -> Result<()> {
    let root = root_dir()?.join(".runs").join(run_id.to_string());
    let tmp = tempfile::tempdir()?;
    let staging = tmp.path().join(format!("run_{run_id}_export"));
    fs::create_dir_all(&staging)?;

    // 1. Copiar archivos según level
    copy_artifact(&root, &staging, "manifest.json")?;
    copy_artifact(&root, &staging, "brief.json")?;
    if level == ExportLevel::Full {
        copy_artifact(&root, &staging, "calls.jsonl")?;
        // ...
    }

    // 2. Calcular SHA256SUMS
    let hashes = sha256_dir(&staging)?;
    fs::write(staging.join("SHA256SUMS"), format_sha256sums(&hashes))?;

    // 3. Crear archivo
    match format {
        ExportFormat::TarGz => tar_gz::create(&staging, &dest)?,
        ExportFormat::Tar   => tar::create(&staging, &dest)?,
        ExportFormat::Zip   => zip::create(&staging, &dest)?,
    }

    Ok(())
}
```

#### 10.9.1. Implementación (v0.3 sub-fase I)

`src/telemetry/export.rs` materializa el bundle en tres
etapas:

1. **Stage**: copia los archivos seleccionados a un
   directorio temporal bajo `tempfile::tempdir()`. El set
   viene de `collect_files(run_dir, level)`:
   - always: `manifest.json`, `brief.json`,
     `rankings/ranking.json`.
   - summary (`+`): `sketches/`, `proposals/`,
     `critiques/`, `revisions/`, `evaluations/`,
     `final/`.
   - full (`+`): `validation/`, `synthesized/`,
     `cluster_proposals/`, `adversaries/`,
     `checkpoints/`, `telemetry/calls.jsonl.gz`,
     `telemetry/phases.jsonl.gz`,
     `telemetry/warnings.jsonl`,
     `telemetry/checkpoints.jsonl`.
   La lista se ordena por path relativo para que el
   `SHA256SUMS` resultante sea estable entre exports
   del mismo run.
2. **Hash**: cada archivo se hashea con `sha2::Sha256` vía
   `sha256_file()` (64 KiB buffer). Las líneas resultantes
   se escriben a `<staging>/SHA256SUMS` en formato
   canónico `<sha256>  <path>\n` (modo binario de
   `sha256sum`). `parse_sha256sums()` parsea
   tolerando CRLF y separadores de espacios múltiples.
3. **Bundle**: el directorio staged se empaca en el
   formato pedido:
   - `tar.gz` con `tar::Builder` + `flate2::GzEncoder`.
   - `tar` con `tar::Builder`.
   - `zip` con `zip::ZipWriter` +
     `SimpleFileOptions::compression_method(Deflated)`.
   `zip` requiere que el writer esté sobre un
   `Write + Seek`, así que `write_zip` usa `File`
   directamente en vez de `BufWriter`.

Tipos públicos:
- `pub struct HashEntry { sha256: String, path: String }`.
- `pub struct ExportResult { archive_path, file_count,
  archive_sha256, payload_bytes, archive_bytes }`.
- `pub fn export_run(run_dir, run_id, level, format, out)
  -> Result<ExportResult>`.

El dispatch de `moagan telemetry export` resuelve el
directorio del run vía `MoaganHome::run_dir(run_id)` y
defaulta el nombre del archivo a
`run_<short-id>_<level>.<ext>` cuando `--out` está
ausente. Errores tempranos (run dir inexistente, format
inválido) devuelven `Error::InvalidArgs` /
`Error::InvalidState` antes de tocar el filesystem.

### 10.10. Verify

```rust
pub fn verify(path: PathBuf) -> Result<()> {
    let sums = fs::read_to_string(path.join("SHA256SUMS"))?;
    for line in sums.lines() {
        let (expected, file) = line.split_once("  ").unwrap();
        let actual = sha256_file(&path.join(file))?;
        if actual != expected {
            return Err(Error::HashMismatch { file: file.into(), expected: expected.into(), actual });
        }
    }
    println!("OK: {} files verified", sums.lines().count());
    Ok(())
}
```

#### 10.10.1. Implementación (v0.3 sub-fase I)

`src/telemetry/verify.rs` consume el bundle producido por
`export.rs` y produce un `VerifyReport`. La función
principal:

```rust
pub fn verify(path: &Path) -> Result<VerifyReport>
```

- Si `path` es un directorio con `SHA256SUMS`, lo
  verifica in-place.
- Si `path` es un archivo `*.tar.gz` / `*.tar` / `*.zip`,
  lo extrae a un `tempfile::tempdir()` antes de leer
  `SHA256SUMS` (las paths relativas del manifest deben
  coincidir con el layout en disco).
- Para cada entry del `SHA256SUMS`:
  - archivo ausente -> `VerifyVerdict::Missing`
  - hash coincide -> `VerifyVerdict::Ok`
  - hash distinto -> `VerifyVerdict::Mismatch { expected,
    actual }`
- `VerifyReport { rows, root }` lleva la lista de
  veredictos + el directorio verificado. `ok_count()` y
  `fail_count()` resumen.

`extract_tar_gz` usa `flate2::MultiGzDecoder` (multi-member
safe, coincide con el `MemberGzWriter` que escribe
`compression.rs::open_gz_append`). `extract_zip` usa
`zip::ZipArchive::by_index` + `enclosed_name()` para
defenderse contra escapes `..`. `extract_tar` usa
`tar::Archive::unpack`.

El CLI dispatch (`moagan telemetry verify --path <path>`)
imprime una línea por archivo (`OK` / `MISSING` /
`MISMATCH  path  expected=…  actual=…`), un resumen final
`OK: N files verified, M failed`, y devuelve
`Error::InvalidState` cuando `M > 0` — sirve como gate
de CI (`set -e` / exit code 1).

### 10.10.2. Retention (`moagan telemetry cleanup`)

Sub-fase I también aterriza la retention pass de V4 §12.
El módulo `src/telemetry/retention.rs` expone:

```rust
pub struct RetentionConfig {
    pub keep_runs_days: u32,
    pub keep_runs_count: u32,
    pub max_storage_bytes: u64,
    pub policy: RetentionPolicy, // Delete | Archive
}

pub struct RetentionCandidate {
    pub run_id: RunId,
    pub path: PathBuf,
    pub bytes: u64,
    pub updated_unix: i64,
}

pub struct RetentionReport {
    pub candidates: Vec<RetentionCandidate>,
    pub total_bytes: u64,
    pub dry_run: bool,
    pub policy: RetentionPolicy,
}

pub fn plan(runs_dir, db_updated: &dyn Fn(RunId) -> Option<i64>,
            cfg) -> Result<RetentionReport>;
pub fn apply(runs_dir, db_updated, cfg, dry_run: bool)
            -> Result<RetentionReport>;
```

Tres filtros componibles (semántica OR — un run es
candidato si falla CUALQUIER keep):
1. **Age**: `keep_runs_days > 0 && now - updated_unix >
   keep_runs_days * 86_400`.
2. **Count**: si `runs.len() > keep_runs_count`, los más
   antiguos (sorted por `updated_unix ASC`) son
   candidatos. `keep_runs_count == 0` significa "keep
   nothing" (útil para el smoke path "delete all").
3. **Storage**: si el running total de bytes (oldest
   first) excede `max_storage_bytes`, los runs que
   contribuyen al overflow son candidatos. `0` desactiva
   el filtro.

Política:
- `Delete`: `std::fs::remove_dir_all(run_dir)`;
  fallos se loguean a stderr y la corrida continúa
  (best-effort).
- `Archive`: `rename(run_dir, archive_root/YYYY-MM-DD/
  <run_id>/)`. `archive_root` = `<root>/archive`. La
  fecha se deriva de `updated_unix` vía el algoritmo
  civil-from-days de Howard Hinnant (sin deps).

El CLI lee `Config::retention` para los knobs:
- `keep_runs_days: 30`
- `keep_runs_count: 100`
- `max_storage_bytes: 50 * 1024 * 1024 * 1024`
- `policy: "delete" | "archive"`

Defaults sensatos que el operador puede sobreescribir
desde `~/.config/moagan/config.toml` (T01-06 §11).

---

## 11. Configuración y arranque

### 11.1. Carga de config

```rust
pub struct Config {
    pub timeouts: Timeouts,
    pub parallelism: ParallelismConfig,
    pub discovery: DiscoveryConfig,
    pub telemetry: TelemetryConfig,
    pub retention: RetentionConfig,
    pub storage: StorageConfig,
    pub server: ServerConfig,
    pub privacy: PrivacyConfig,
    pub security: SecurityConfig,
    pub api_keys: HashMap<String, String>,
    pub providers: HashMap<String, ProviderConfig>,
}

pub fn load_config() -> Result<Config> {
    let dotenv = dotenvy::dotenv();
    let _ = dotenv; // no-op si no existe

    let path = config_path()?; // ~/.config/moagan/config.toml
    let raw = fs::read_to_string(&path).unwrap_or_default();
    let cfg: Config = toml::from_str(&raw)?;

    validate(&cfg)?;
    Ok(cfg)
}
```

### 11.2. Override por CLI

`clap` con `#[arg(env)]` permite que cada flag se pueda pasar por env (`MOAGAN_PARALLELISM_MAX=8 moagan run ...`). El orden de precedencia:

1. Flags CLI.
2. Env vars.
3. `~/.config/moagan/config.toml`.
4. Defaults hardcoded.

### 11.3. `.env.example`

```text
# moagan — example environment file
# Copy to .env and fill in. Never commit .env.

MOAGAN_HOME=~/.local/share/moagan

MINIMAX_API_KEY=
GLM_API_KEY=
QWEN_API_KEY=
KIMI_API_KEY=
DEEPSEEK_API_KEY=
OPENCODE_GO_API_KEY=

MOAGAN_PARALLELISM_MAX=4
MOAGAN_TIMEOUT_TOTAL=0
MOAGAN_TELEMETRY_LEVEL=full
```

### 11.4. Defaults

```rust
impl Default for Config {
    fn default() -> Self {
        Self {
            timeouts: Timeouts {
                sketch: Duration::from_secs(120),
                phase:  Duration::ZERO,
                total:  Duration::ZERO,
            },
            parallelism: ParallelismConfig {
                sketch: 4,
                phase: 4,
                extraction: 4,
                max_parallelism: 4,
            },
            discovery: DiscoveryConfig {
                max_categorias_default: 12,
                min_ejemplos_por_categoria: 5,
                max_categorias_soft: 30,
                reserve_ratio: 0.25,
                uncategorized_threshold: 0.3,
            },
            telemetry: TelemetryConfig {
                level: "full".into(),
                warning_threshold: 0.8,
                hard_limit: 0.95,
            },
            retention: RetentionConfig {
                keep_runs_days: 30,
                keep_runs_count: 100,
                max_storage_bytes: 50 * 1024 * 1024 * 1024,
                policy: "delete".into(),
            },
            storage: StorageConfig {
                jsonl_compression: "gz".into(),
                manifest_compression: "none".into(),
            },
            server: ServerConfig {
                port: 4096,
                port_search_max: 1000,
                port_blacklist: vec![22, 80, 443, 3306, 5432, 6379, 8080, 8443],
            },
            privacy: PrivacyConfig {
                redact_in_brief: false,
                redact_in_prompts: false,
                redact_in_telemetry: true,
                redact_in_storage: true,
                redact_in_export: true,
                attribute_model: true,
                attribute_provider: true,
                attribute_version: true,
                redact_patterns: default_patterns(),
            },
            security: SecurityConfig {
                api_key_default_input: "interactive".into(),
            },
            api_keys: HashMap::new(),
            providers: HashMap::new(),
        }
    }
}
```

---

## 12. Errores

### 12.1. Tipos

```rust
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("timeout after {0:?}")]
    Timeout(Duration),

    #[error("cancelled")]
    Cancelled,

    #[error("budget exhausted in phase {0}")]
    BudgetExhausted(String),

    #[error("plan paused: {0}")]
    PlanPaused(String),

    #[error("api_key_invalid: provider {0}")]
    ApiKeyInvalid(String),

    #[error("schema violation in role {0}: {1}")]
    SchemaViolation(String, String),

    #[error("hash mismatch: file {file}, expected {expected}, got {actual}")]
    HashMismatch { file: String, expected: String, actual: String },

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid state: {0}")]
    InvalidState(String),
}
```

### 12.2. Política de panics

- **Cero panics en producción.** Todo `unwrap` reemplazado por `expect` con mensaje contextual, o propagado.
- Tests pueden usar `unwrap`.

### 12.3. Códigos de salida

```text
0   ok
1   error genérico
2   argumentos inválidos
3   api key inválida
4   plan exhausted
5   timeout
6   cancelled
7   schema violation persistente
8   io error
```

---

## 13. Logging

`tracing` con `EnvFilter`:

```rust
tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,moagan=debug")))
    .json()
    .with_writer(||
        Box::new(RedactWriter::new(std::io::stderr()))
    )
    .init();
```

`RedactWriter` envuelve cualquier `io::Write` y aplica redacción antes de escribir.

---

## 14. Política de tests

### 14.1. Unit tests

Dentro de cada módulo (`#[cfg(test)] mod tests`). Cobertura objetivo:

- `redact/apply.rs`: 100%.
- `ids.rs`: 100%.
- `time.rs`: 100%.
- `ranking/pareto.rs`: 100%.
- `llm/cache.rs`: 100%.
- `ranking/stability.rs`: 100%.

### 14.2. Integration tests

- `tests/integration_mvp.rs`: corre el modo `standard` con `mock` provider. Verifica que el `ranking.json` se genera.
- `tests/integration_discovery.rs`: corre `discovery` con 8 sketches ficticios. Verifica que `final/cat_*.md` existe.
- `tests/integration_continue.rs`: corre un run, lo aborta con `Ctrl-C`, llama a `continue`, verifica reanudación.
- `tests/integration_provider_switch.rs`: corre un run, simula `pause`, verifica `provider_changes` y continuation con nuevo provider.

### 14.3. Fixtures

`tests/fixtures/` contiene:

- `mock_provider/responses/*.json` — respuestas pre-canned.
- `runs/sample_run/` — un run completo generado previamente para `inspect`/`import`.
- `schemas/` — schemas JSON embebidos.

### 14.4. Mock provider

```rust
// src/llm/mock.rs
pub struct MockProvider {
    pub responses: Vec<MockResponse>,
    pub index: AtomicUsize,
}

impl MockProvider {
    pub fn from_dir(path: &Path) -> Result<Self> { ... }
}

#[async_trait]
impl Provider for MockProvider {
    async fn send(&self, req: &Request) -> Result<Response> {
        let i = self.index.fetch_add(1, Ordering::SeqCst);
        let r = self.responses.get(i).ok_or(Error::MockExhausted)?;
        Ok(r.clone().into())
    }
}
```

Las tests usan `MockProvider` en lugar de HTTP real. `wiremock` está disponible para tests que sí quieran servidor HTTP.

---

## 15. Manejo de providers

### 15.1. Registro

```rust
pub struct ProviderPool {
    by_name: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderPool {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let mut r = Self { by_name: HashMap::new() };
        for (name, spec) in &cfg.providers {
            let provider: Arc<dyn Provider> = match name.as_str() {
                "minimax"    => Arc::new(MiniMaxProvider::new(spec, &cfg.api_keys)?),
                "glm"        => Arc::new(GlmProvider::new(spec, &cfg.api_keys)?),
                "qwen"       => Arc::new(QwenProvider::new(spec, &cfg.api_keys)?),
                "kimi"       => Arc::new(KimiProvider::new(spec, &cfg.api_keys)?),
                "deepseek"   => Arc::new(DeepSeekProvider::new(spec, &cfg.api_keys)?),
                "opencode_go"=> Arc::new(OpencodeGoProvider::new(spec, &cfg.api_keys)?),
                "mock"       => Arc::new(MockProvider::empty()),
                _ => return Err(Error::InvalidProvider(name.clone())),
            };
            r.by_name.insert(name.clone(), provider);
        }
        Ok(r)
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.by_name.get(name).cloned()
    }
}
```

### 15.2. Implementación por provider

Cada provider implementa:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn endpoint(&self) -> &str;
    fn model(&self) -> &str;
    fn supports_token_plan(&self) -> bool;
    fn current_usage(&self) -> Usage;
    fn update_usage(&self, usage: Usage);
    async fn send(&self, req: &Request) -> Result<Response>;
    async fn identity(&self) -> Result<ModelIdentity>;
}
```

Los providers comparten código en `llm/http.rs::request<T: serde::de::DeserializeOwned>(method, url, headers, body) -> Result<T>`. Sólo difieren en:

- Construcción del request body (esquema nativo del provider).
- Parseo del response (llamada a `parse_usage`).
- Manejo de errores.

### 15.3. Manejo de plan

```rust
pub struct PlanTracker {
    pub plan_type: String,
    pub plan_limit: u64,
    pub warning_threshold: f32,
    pub hard_limit: f32,
    pub used: AtomicU64,
}

impl PlanTracker {
    pub fn state(&self) -> PlanState {
        let pct = self.used.load(Ordering::SeqCst) as f32 / self.plan_limit as f32;
        if pct >= self.hard_limit { PlanState::Paused }
        else if pct >= self.warning_threshold { PlanState::Warning }
        else { PlanState::Normal }
    }

    pub fn check_and_reserve(&self, tokens: u64) -> Result<()> {
        match self.state() {
            PlanState::Paused => Err(Error::PlanPaused(self.name())),
            PlanState::Warning | PlanState::Normal => {
                self.used.fetch_add(tokens, Ordering::SeqCst);
                Ok(())
            }
        }
    }
}
```

El `PlanTracker` se actualiza en `phase.end` con los tokens consumidos.

### 15.4. Hibernación

```rust
pub async fn handle_pause(manifest: &mut Manifest, db: &Db) -> Result<()> {
    manifest.status = "paused";
    manifest.paused_at = Some(now());
    manifest.pause_reason = Some("plan_exceeded".into());
    manifest.pause_user_notified = true;
    manifest.save()?;
    db.update_run_status(&manifest)?;

    // Menú vía dialoguer
    let opts = vec![
        "Esperar reset del plan",
        "Cambiar provider",
        "Cambiar api-key",
        "Reducir paralelismo",
        "Reducir cardinalidad",
        "Cancelar run",
    ];
    let sel = dialoguer::Select::new().items(&opts).default(0).interact()?;
    match sel {
        0 => {} // espera
        1 => { /* cambia provider */ }
        2 => { /* cambia api-key */ }
        3 => { /* reduce parallelism */ }
        4 => { /* reduce cardinality */ }
        5 => { manifest.status = "cancelled"; manifest.save()?; }
        _ => unreachable!(),
    }
    Ok(())
}
```

---

## 16. Persistencia por fase

### 16.1. Intake

```rust
pub async fn intake(ctx: &mut RunContext, raw: &str) -> Result<RunContext> {
    let normalized = normalize::normalize(raw);
    let detected = detect::detect(&normalized);
    let injection_state = detect::injection::analyze(&normalized);

    let run_id = new_run_id();
    let mut ctx = RunContext {
        run_id,
        input_hash: sha256_hex(&normalized),
        raw_prompt: raw.to_string(),
        normalized_prompt: normalized,
        attachments: vec![],
        detected_language: detected.language,
        explicit_constraints: detected.constraints,
        requested_artifacts: detected.artifacts,
        budget: ctx.budget.clone(),
        execution_policy: ctx.execution_policy.clone(),
        enabled_models: ctx.enabled_models.clone(),
        enabled_roles: ctx.enabled_roles.clone(),
        previous_feedback: vec![],
        injection_safety_state: injection_state,
    };

    ctx.save()?;
    Ok(ctx)
}
```

`save()` escribe `brief.json` (parcial, se completa en `clarify`) y `manifest.json`.

### 16.2. Clarify

```rust
pub async fn clarify(ctx: &RunContext, cancel: CancellationToken) -> Result<CanonicalBrief> {
    let role = "clarify";
    let prompt = render(role, ctx)?;
    let req = Request::new(role, &prompt, ctx.budget.intake)?;
    let resp = ctx.provider.send(req).await?;
    let parsed: ClarifyOutput = validate_json(&resp, schema_for(role))?;

    let mut brief = CanonicalBrief::from_clarify(parsed, &ctx);

    if brief.has_blocking_ambiguity() || brief.has_hard_contradiction() || brief.risk_level == "high" {
        let ckp = Checkpoint::clarify(&brief);
        let res = human::ask(&ckp).await?;
        brief.apply_resolution(res)?;
    }

    brief.save_to(ctx)?;
    Ok(brief)
}
```

### 16.3. Route

```rust
pub async fn route(brief: &CanonicalBrief, policy: &ExecutionPolicy) -> Result<Mode> {
    // 1. Modo explícito tiene prioridad
    if let Some(m) = policy.mode {
        return Ok(m);
    }

    // 2. Llamada al router
    let role = "router";
    let prompt = render(role, brief)?;
    let req = Request::new(role, &prompt, 0)?;
    let resp = provider_for(role).send(req).await?;
    let parsed: RouterOutput = validate_json(&resp, schema_for(role))?;

    let mode = match parsed.recommendation.as_str() {
        "fast" | "standard" | "deep" | "explore" | "batch" | "discovery" => {
            Mode::from_str(&parsed.recommendation)?
        }
        _ => Mode::Standard,
    };
    Ok(mode)
}
```

### 16.4. Decompose

```rust
pub async fn decompose(brief: &CanonicalBrief, db: &Db) -> Result<ProblemGraph> {
    if !should_decompose(brief) {
        return Ok(ProblemGraph::trivial(brief));
    }

    let role = "decomposer";
    let prompt = render(role, brief)?;
    let req = Request::new(role, &prompt, brief.budget.decomposition)?;
    let resp = provider_for(role).send(req).await?;
    let parsed: DecomposeOutput = validate_json(&resp, schema_for(role))?;

    let graph = ProblemGraph::from(parsed);
    graph.save_to(brief.run_id)?;
    Ok(graph)
}
```

#### 16.4.1. Implementación real (v0.3 sub-fase G, 2026-07-31)

Aterrizado en `src/phases/decompose.rs` + `src/domain.rs` + la
migración SQLite v006. El código real difiere del esqueleto
arriba en tres puntos:

1. **Sidecar atómico**: el grafo se persiste vía
   `AtomicWriter::new().write(...)` para que un crash a
   mitad de escritura no deje un `problem_graph.json` parcial.
   El mirror a SQLite es best-effort y nunca aborta la fase.

2. **DAG repair**: la fase valida el grafo con Kahn; un
   grafo con ciclos o dependencias colgantes se repara
   eliminando los nodos ofensivos (cap a 8 rondas). Si todo
   se cae, vuelve al `ProblemGraph::trivial(...)`. La
   función pura `repair(&mut ProblemGraph) -> Result<...>`
   está cubierta por 6 unit tests en
   `src/phases/decompose.rs::tests`.

3. **`should_decompose` ladder**: la función pura
   `domain::should_decompose(&Brief) -> bool` implementa la
   escalera del §5.3 (≥3 constraints, ≥3 deliverables, magic
   words `subproblem`/`phase`/`depends on`/`after`/`once`).
   La función tiene 1 unit test que cubre cada rama.

#### 16.4.2. Topological layers

`ProblemGraph::topological_layers() -> Result<Vec<Vec<usize>>, String>`
usa el algoritmo de Kahn. Las capas se devuelven en orden
de precedencia (capa 0 = nodos raíz sin dependencias, capa 1
= hijos de la capa 0, etc.). El método `validate_no_cycles`
es un thin wrapper que falla con `Err("graph has a cycle;
stuck at: [...]")` cuando el grafo no es un DAG.

`roots()` devuelve los índices de los nodos sin padres, útil
para que `SketchPhase` arranque la distribución por el
conjunto correcto.

#### 16.4.3. Wiring

`DecomposePhase` se inserta en `build_pipeline_for_mode` SOLO
cuando `mode == Mode::Deep` (T01-06 §8.1 «sólo deep»). El
resto de modos no insertan la fase y pagan 0 overhead.
El vector de pipeline para `Mode::Deep` queda:

```
intake -> clarify -> route -> decompose -> sketch -> propose
  -> validate -> cluster_proposals -> synthesize
  -> gate -> critique -> repair -> judge -> rank -> deliver
```

`SketchPhase` consume el sidecar cuando existe y es no-trivial.
La función `distribute_across_nodes(count, &node_ids)` reparte
el conteo de modo que cada nodo recibe al menos un sketch.
El campo `Sketch.angle` se rellena con el id del nodo (en
lugar del ángulo humano) para que la cache key siga distinta
por nodo.

### 16.5. Sketch

```rust
pub async fn sketch_phase(
    brief: &CanonicalBrief,
    graph: &ProblemGraph,
    parallelism: &Parallelism,
    cancel: CancellationToken,
) -> Result<Vec<Sketch>> {
    let cells = graph.sketch_cells();
    let sem = parallelism.semaphore();
    let tasks = cells.into_iter().map(|cell| {
        let sem = sem.clone();
        let brief = brief.clone();
        let cancel = cancel.clone();
        async move {
            let permit = sem.acquire().await?;
            let role = "sketcher";
            let prompt = render(role, (&brief, &cell))?;
            let req = Request::new(role, &prompt, brief.budget.sketches / cells.len() as u64)?;
            let resp = match timeout(brief.timeouts.sketch, provider_for(role).send(req)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(Error::Timeout(brief.timeouts.sketch)),
            };
            let parsed: SketchOutput = validate_json(&resp, schema_for(role))?;
            let sk = Sketch::from(parsed, cell)?;
            sk.save_to(brief.run_id)?;
            drop(permit);
            Ok(sk)
        }
    });

    let results = futures::future::join_all(tasks).await;
    let sketches: Vec<_> = results.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(sketches)
}
```

### 16.6. Proposal

```rust
pub async fn proposal_phase(
    brief: &CanonicalBrief,
    selected_sketches: &[Sketch],
    parallelism: &Parallelism,
) -> Result<Vec<Proposal>> {
    let sem = parallelism.semaphore();
    let tasks = selected_sketches.iter().map(|sk| {
        let sem = sem.clone();
        let brief = brief.clone();
        let sk = sk.clone();
        async move {
            let permit = sem.acquire().await?;
            let role = "proposer";
            let prompt = render(role, (&brief, &sk))?;
            let req = Request::new(role, &prompt, brief.budget.full_proposals)?;
            let resp = provider_for(role).send(req).await?;
            let parsed: ProposalOutput = validate_json(&resp, schema_for(role))?;
            let p = Proposal::from(parsed, &sk)?;
            p.save_to(brief.run_id)?;
            drop(permit);
            Ok(p)
        }
    });

    let results = futures::future::join_all(tasks).await;
    let proposals: Vec<_> = results.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(proposals)
}
```

### 16.7. Gate

```rust
pub fn gate(p: &Proposal) -> GateStatus {
    let report = validators::structural::check(p);
    if report.has_hard_violation() { return GateStatus::Fail; }
    if report.has_minor_violation() { return GateStatus::Warn; }
    GateStatus::Pass
}
```

### 16.8. Validate

```rust
pub async fn validate(p: &Proposal, sandbox: &Sandbox) -> Result<ValidationEvidence> {
    let mut evidence = ValidationEvidence::default();
    for artifact in &p.artifacts {
        let v = match artifact.kind.as_str() {
            "rust" => validators::rust_validator::check(artifact, sandbox).await?,
            "python" => validators::python_validator::check(artifact, sandbox).await?,
            "typescript" => validators::typescript_validator::check(artifact, sandbox).await?,
            _ => continue,
        };
        evidence.merge(v);
    }
    evidence.save_to(p)?;
    Ok(evidence)
}
```

### 16.9. Critique

```rust
pub async fn critique_phase(p: &Proposal, db: &Db) -> Result<Vec<Critique>> {
    let roles = critic_assignment(p);
    let tasks = roles.iter().map(|role| {
        let p = p.clone();
        async move {
            let prompt = render(role, &p)?;
            let req = Request::new(role, &prompt, 0)?;
            let resp = provider_for(role).send(req).await?;
            let parsed: CritiqueOutput = validate_json(&resp, schema_for(role))?;
            let c = Critique::from(parsed, role, &p)?;
            c.save_to(&p)?;
            Ok(c)
        }
    });
    let results = futures::future::join_all(tasks).await;
    let critiques: Vec<_> = results.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(critiques)
}
```

### 16.10. Repair

```rust
pub async fn repair_phase(
    p: &Proposal,
    critiques: &[Critique],
    budget: &RepairBudget,
) -> Result<Proposal> {
    let mut current = p.clone();
    let mut rounds = 0;
    while rounds < budget.max_rounds {
        let blockers = current.blockers(critiques);
        if blockers.is_empty() { break; }

        let role = "repairer";
        let prompt = render(role, (&current, blockers, critiques))?;
        let req = Request::new(role, &prompt, budget.per_round)?;
        let resp = provider_for(role).send(req).await?;
        let parsed: ProposalOutput = validate_json(&resp, schema_for(role))?;
        let revised = Proposal::from(parsed, &current)?;

        revised.save_to(&current)?;
        current = revised;
        rounds += 1;
    }
    Ok(current)
}
```

### 16.11. Judge

```rust
pub async fn judge_phase(p: &Proposal, db: &Db) -> Result<Evaluation> {
    let judges = vec!["judge_correctness", "judge_completeness", "judge_feasibility"];
    let tasks = judges.iter().map(|role| {
        let p = p.clone();
        async move {
            let prompt = render(role, &p)?;
            let req = Request::new(role, &prompt, 0)?;
            let resp = provider_for(role).send(req).await?;
            let parsed: EvaluationOutput = validate_json(&resp, schema_for(role))?;
            Ok((role.to_string(), parsed))
        }
    });
    let results = futures::future::join_all(tasks).await;
    let raw: Vec<_> = results.into_iter().collect::<Result<Vec<_>>>()?;
    let eval = Evaluation::aggregate(raw, &p)?;
    eval.save_to(&p)?;
    Ok(eval)
}
```

### 16.12. Rank

```rust
pub fn rank_phase(proposals: &[Proposal], evaluations: &[Evaluation]) -> Result<Ranking> {
    let vectors: Vec<_> = proposals.iter().zip(evaluations.iter())
        .map(|(p, e)| (p.id, e.quality_vector()))
        .collect();

    let pareto = ranking::pareto::pareto_front(&vectors);
    let clusters = ranking::cluster::cluster_by_stack(proposals, &pareto);
    let representatives = ranking::diversity::pick_with_crowding(clusters, 3);
    let ranking = ranking::aggregate::rank(&vectors, &representatives, &Weights::default())?;

    let stable = ranking::stability::stability_score(&vectors, &Weights::default(), 50);
    let r = Ranking { pareto, representatives, ranking, stability: stable };
    r.save()?;
    Ok(r)
}
```

---

## 17. Orden concreto de llamadas LLM

| Modo | Fases que llaman LLM | Cardinalidad típica |
|---|---|---|
| `fast` | intake, clarify, proposal×2, critique×2, judge×1 | 6 |
| `standard` | intake, clarify, sketch×4, proposal×3, critique×6, repair×1, judge×3, adversary×1 | 20 |
| `deep` | intake, clarify, decompose, sketch×5, proposal×5, critique×10, repair×2, judge×3, adversary×1 | 31 |
| `explore` | intake, clarify, sketch×10, tagger×10, cluster×0, extract×N, integrate×N | 30+ |
| `batch` | igual que standard, sin pausas | 20 |
| `discovery` | intake, clarify, matrix×N, tagger×N, contradiction×N, facet×N, extract×N, integrate×N | 40–500 |

Nota: `intake` puede ser opcional si la normalización es totalmente local. Por simplicidad, todas las fases hacen una llamada LLM para mantener consistencia.

---

## 18. Estrategia de revisión

### 18.1. Revisión adversaria

```rust
pub async fn adversary(p: &Proposal, ranking: &Ranking) -> Result<AdversarialReport> {
    let role = "adversary";
    let prompt = render(role, (p, ranking))?;
    let req = Request::new(role, &prompt, 0)?;
    let resp = provider_for(role).send(req).await?;
    let parsed: AdversarialOutput = validate_json(&resp, schema_for(role))?;
    let r = AdversarialReport::from(parsed);
    r.save_to(p)?;
    Ok(r)
}
```

### 18.2. Iteración localizada

```rust
pub enum RefineAction {
    Focus { proposal_id: Uuid, focus: String },
    Expand { proposal_id: Uuid, section: String },
    Variant { proposal_id: Uuid, hint: String },
    Rerank { weights: HashMap<String, f32> },
    Critique { proposal_id: Uuid, lens: String },
    Synthesize { cluster_id: String },
    Reframe { add_constraint: String },
}
```

Cada acción invalida selectivamente artefactos downstream:

```rust
fn invalidate_downstream(p: &Proposal, action: &RefineAction) -> Vec<PathBuf> {
    let mut to_delete = vec![];
    match action {
        RefineAction::Focus { proposal_id, .. } => {
            // Eliminar critiques, revisions, evaluations, ranking
            to_delete.push(format!("critiques/p_{}_*.json", proposal_id));
            to_delete.push(format!("revisions/p_{}_*.json", proposal_id));
            to_delete.push(format!("evaluations/p_{}.json", proposal_id));
            to_delete.push("rankings/ranking.json".into());
        }
        RefineAction::Rerank { .. } => {
            to_delete.push("rankings/ranking.json".into());
        }
        _ => {}
    }
    to_delete
}
```

---

## 19. Privacidad operacional

### 19.1. Redacción al escribir

Cualquier función `write_*` pasa por `redact::apply`. Esto se implementa con un wrapper:

```rust
pub struct RedactWriter<W: Write> { inner: W }
impl<W: Write> Write for RedactWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let s = String::from_utf8_lossy(buf);
        let redacted = redact::apply(&s);
        self.inner.write(redacted.as_bytes())
    }
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}
```

### 19.2. Redacción de errores

```rust
pub fn write_error(db: &Db, call_id: &str, raw_msg: &str) -> Result<()> {
    let redacted = redact::apply(raw_msg);
    db.update_call_error(call_id, raw_msg, &redacted)?;
    Ok(())
}
```

### 19.3. API keys nunca en logs

```rust
pub fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "*".repeat(key.len());
    }
    let prefix = &key[..4];
    let last = &key[key.len()-4..];
    format!("{}...{}", prefix, last)
}
```

### 19.4. `api_key_ref` en manifest

El manifest guarda `api_key_ref: "env:MINIMAX_API_KEY"`, nunca el valor. La resolución al valor ocurre en memoria, no en disco.

---

## 20. Comportamiento del `Parallelism`

### 20.1. Adquisición por fase

```rust
pub async fn run_phase<P: Phase>(parallelism: &Parallelism, phase: &P) -> Result<P::Output> {
    let desired = phase.desired_parallelism();
    let free = parallelism.max() - parallelism.in_use();
    let granted = desired.min(free).max(1);
    let permits = parallelism.acquire_many(granted as u32).await?;
    let result = phase.execute(permits).await;
    drop(permits);
    result
}
```

### 20.2. Tracking

`in_use` se incrementa antes de ejecutar la tarea, se decrementa después. `Semaphore` interno lo gestiona.

### 20.3. Saturación

Cuando `in_use == max` durante más de `5s`, se registra `saturation_event` en `phases`:

```rust
if parallelism.in_use() == parallelism.max() {
    saturation_events.fetch_add(1, Ordering::SeqCst);
}
```

Esto se materializa en `telemetry/phases.jsonl`.

---

## 21. Orden de operaciones en una reanulación

```text
moagan continue run_disc
  ↓
1. Cargar manifest.json del run_id
2. Validar estado: status ∈ {paused, running, created}
3. Si status=created, marcar running
4. Encontrar última fase completada
5. Si --switch-provider:
   a. Validar provider existe
   b. Actualizar manifest.provider
   c. Registrar provider_change en SQLite
   d. NO eliminar artefactos previos
6. Si --switch-api-key:
   a. Resolver (interactive | env: | file:)
   b. Actualizar manifest.api_key_ref
   c. NO escribir el valor
7. Continuar pipeline desde la fase siguiente
```

---

## 22. Versionado del schema de migración

Si en el futuro se necesita v002:

```rust
const MIGRATIONS: &[(&str, &str)] = &[
    ("v001_initial.sql", include_str!("migrations/v001_initial.sql")),
    ("v002_provider_changes.sql", include_str!("migrations/v002_provider_changes.sql")),
];
```

Cada migración es idempotente (`CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`).

`PRAGMA user_version` se incrementa con `N` después de aplicar la migración `N`.

---

## 23. Modo `batch` (no interactivo)

```rust
pub struct BatchPolicy {
    pub auto_accept_clarify: bool,
    pub skip_final_checkpoint: bool,
    pub fail_on_blocking_ambiguity: bool,
    pub output_format: BatchOutputFormat, // JSON estable
}

pub enum BatchOutputFormat {
    JsonStable,
    JsonLines,
}
```

Si `fail_on_blocking_ambiguity=true` y hay ambigüedad bloqueante, el run termina con `status='failed'` y `reason='NeedsInput'`.

El output `summary.json` se escribe en `final/summary.json` con un schema estable:

```json
{
  "schema_version": "v1",
  "run_id": "...",
  "mode": "batch",
  "started_at": "...",
  "ended_at": "...",
  "selected": [{ "id": "p_...", "score": 0.87 }],
  "alternatives": [{ "id": "p_..." }],
  "rejected": [],
  "ranking_input": {
    "weights": {},
    "perturbations": 50
  }
}
```

---

## 24. Testing del sandbox

```rust
#[tokio::test]
async fn test_rust_validator() {
    let sandbox = Sandbox::new(Duration::from_secs(30), vec!["cargo".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2024\"\n").unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src").join("main.rs"), "fn main() { println!(\"hi\"); }").unwrap();

    let result = sandbox.run("cargo", &["check"]).await.unwrap();
    assert_eq!(result.exit_code, 0);
}
```

---

## 25. Códigos de error del LLM clasificados

```rust
pub fn classify_error(http_status: u16, body: &str) -> &'static str {
    match http_status {
        200 => "OK",
        400 => "BAD_REQUEST",
        401 => "UNAUTHORIZED",
        403 => "FORBIDDEN",
        404 => "NOT_FOUND",
        408 => "REQUEST_TIMEOUT",
        413 => "PAYLOAD_TOO_LARGE",
        429 => "HTTP_429",
        500 => "HTTP_500",
        502 => "HTTP_502",
        503 => "HTTP_503",
        504 => "HTTP_504",
        _ => "UNKNOWN_HTTP",
    }
}
```

`TRANSPORT_ERROR` se usa cuando `reqwest::Error` no es HTTP (red rota, TLS, etc.).

---

## 26. Decisiones de implementación específicas por provider

### 26.1. MiniMax (`minimax`)

```rust
pub struct MiniMaxProvider {
    endpoint: String,
    model: String,
    api_key: String,
    client: reqwest::Client,
    plan: Option<PlanTracker>,
}

impl MiniMaxProvider {
    pub fn new(spec: &ProviderConfig, api_keys: &HashMap<String, String>) -> Result<Self> {
        let api_key = resolve_api_key(&spec.api_key_ref, api_keys)?;
        Ok(Self {
            endpoint: "https://api.minimax.io/anthropic/v1/messages".into(),
            model: "MiniMax-M3".into(),
            api_key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            plan: spec.plan.clone().map(PlanTracker::from),
        })
    }
}

#[async_trait]
impl Provider for MiniMaxProvider {
    async fn send(&self, req: &Request) -> Result<Response> {
        let body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "top_p": req.top_p,
            "messages": [{"role": "user", "content": req.prompt}],
        });
        let resp = self.client.post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await?;
        let usage = parse_anthropic_usage(&body);
        let text = body["content"][0]["text"].as_str().unwrap_or("").to_string();
        Ok(Response { text, usage, http_status: status, error: None })
    }
}
```

### 26.2. GLM, Qwen, Kimi, DeepSeek, Opencode_GO

Cada uno encapsula:

- Endpoint distinto.
- Header de auth distinto.
- Esquema de request distinto.
- Esquema de response distinto.

Todos heredan de `llm/http.rs::HttpProvider` (struct que tiene `endpoint`, `api_key`, `client`) y overridean:

```rust
async fn build_body(&self, req: &Request) -> serde_json::Value;
fn parse_response(&self, body: &serde_json::Value) -> Response;
```

### 26.3. Mock

```rust
pub struct MockProvider {
    pub fixtures: Vec<MockResponse>,
    pub index: AtomicUsize,
}

impl MockProvider {
    pub fn from_dir(path: &Path) -> Result<Self> {
        let mut fixtures = vec![];
        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            let text = fs::read_to_string(&path)?;
            let resp: MockResponse = serde_json::from_str(&text)?;
            fixtures.push(resp);
        }
        Ok(Self { fixtures, index: AtomicUsize::new(0) })
    }
}
```

`MockResponse`:

```json
{
  "match": { "role": "sketcher", "phase": "sketch_phase" },
  "http_status": 200,
  "body": {
    "content": [{"type": "text", "text": "{\"thesis\":\"...\"}"}],
    "usage": {"input_tokens": 100, "output_tokens": 200}
  }
}
```

Si el role+fase no matchea, se aborta con `Error::MockNoMatch`.

---

## 27. Telemetría: redacción al escribir

`telemetry::append_phase(path: &Path, event: PhaseEvent)`:

```rust
pub fn append_phase(path: &Path, event: &PhaseEvent) -> Result<()> {
    let json = serde_json::to_string(event)?;
    let redacted = redact::apply(&json);
    let line = format!("{}\n", redacted);
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = RedactWriter::new(file);
    writer.write_all(line.as_bytes())?;
    Ok(())
}
```

El `RedactWriter` se usa en TODA escritura para garantizar consistencia.

---

## 28. Sincronización entre filesystem y SQLite

### 28.1. Reconciliación al inicio

`moagan continue` y `moagan resume` ejecutan:

```rust
pub fn reconcile(run_id: Uuid) -> Result<()> {
    let root = root_dir()?.join(".runs").join(run_id.to_string());
    let manifest = Manifest::load(&root)?;

    // 1. Contar archivos en filesystem
    let sketches_on_disk = count_files(&root.join("sketches"))?;
    let proposals_on_disk = count_files(&root.join("proposals"))?;
    let critiques_on_disk = count_files(&root.join("critiques"))?;

    // 2. Contar filas en SQLite
    let sketches_in_db = db.count_sketches(run_id)?;
    let proposals_in_db = db.count_proposals(run_id)?;

    // 3. Si difieren, re-indexar
    if sketches_on_disk != sketches_in_db {
        db.reindex_sketches(run_id, &root)?;
    }
    if proposals_on_disk != proposals_in_db {
        db.reindex_proposals(run_id, &root)?;
    }

    // 4. Comparar fases.jsonl.gz con tabla phases
    let jsonl_phases = read_phases_jsonl(&root.join("telemetry").join("phases.jsonl.gz"))?;
    let db_phases = db.get_phases(run_id)?;
    if jsonl_phases.len() != db_phases.len() {
        db.reindex_phases(run_id, jsonl_phases)?;
    }

    Ok(())
}
```

### 28.2. Escritura atómica

Cada sidecar se escribe con un patrón atómico:

```rust
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
```

Esto evita archivos parciales.

---

## 29. Hardening específico

### 29.1. Validación de paths

Cualquier path que venga de un LLM (artifact.path, output dir) se valida:

```rust
pub fn safe_path(root: &Path, candidate: &str) -> Result<PathBuf> {
    let p = root.join(candidate);
    let canonical = p.canonicalize().unwrap_or(p.clone());
    if !canonical.starts_with(root) {
        return Err(Error::InvalidState(format!("path escapes root: {}", candidate)));
    }
    Ok(p)
}
```

### 29.2. Tamaño máximo

- `prompt.max_bytes`: 250 KB.
- `response.max_bytes`: 10 MB.
- `attachment.max_bytes`: 50 MB.

Si se excede, `error_code='PAYLOAD_TOO_LARGE'`.

### 29.3. Token estimation

```rust
pub fn estimate_tokens(text: &str) -> u64 {
    // Heurística: ~4 chars = 1 token (inglés), ~2 chars (CJK)
    let mut chars = 0;
    for c in text.chars() {
        chars += if (c as u32) < 0x80 { 1 } else { 2 };
    }
    (chars / 4) as u64
}
```

Esta estimación se usa para el `Budget` antes de la llamada.

---

## 30. Configuración del `Pipeline`

```rust
pub struct Pipeline {
    pub manifest: Manifest,
    pub db: Db,
    pub parallelism: Arc<Parallelism>,
    pub cancel: CancellationToken,
    pub phases: Vec<Box<dyn Phase>>,
}
```

`Pipeline::run`:

```rust
pub async fn run(&mut self) -> Result<()> {
    let mode = self.manifest.mode.clone();
    self.phases = build_phases(&mode, &self.manifest)?;
    for phase in &self.phases {
        if self.cancel.is_cancelled() { break; }
        let phase_name = phase.name();
        telemetry::ring_phase::start(&self.db, &self.manifest.run_id, phase_name).await?;
        match phase.execute(&self.manifest, &self.db, self.parallelism.clone(), self.cancel.clone()).await {
            Ok(out) => {
                telemetry::ring_phase::end(&self.db, &self.manifest.run_id, phase_name, &out).await?;
            }
            Err(e) => {
                telemetry::ring_phase::error(&self.db, &self.manifest.run_id, phase_name, &e).await?;
                if phase.is_critical() {
                    self.manifest.status = "failed";
                    self.manifest.save()?;
                    return Err(e);
                }
            }
        }
    }
    self.manifest.status = "completed";
    self.manifest.save()?;
    Ok(())
}
```

---

## 31. CLI: estructura clap

```rust
#[derive(Parser)]
#[command(name = "moagan", version, about = "Mixture of Agents orchestrator")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    Run {
        #[arg(long, value_enum)]
        mode: Mode,
        // ... args
    },
    Continue {
        run_id: Option<Uuid>,
        #[arg(long)]
        skip_checkpoint: bool,
        #[arg(long)]
        switch_provider: Option<String>,
        #[arg(long)]
        switch_api_key: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    Resume {
        run_id: Uuid,
    },
    Rerun {
        run_id: Uuid,
        #[arg(long)]
        same_config: bool,
        #[arg(long)]
        matrix_override: Option<String>,
    },
    Inspect {
        run_id: Uuid,
        #[arg(long)]
        phase: Option<String>,
    },
    Import {
        source_path: PathBuf,
    },
    Telemetry {
        #[command(subcommand)]
        cmd: TelemetryCmd,
    },
}
```

---

## 32. Manejo del `Budget`

```rust
pub struct Budget {
    pub slots: HashMap<String, u64>,
    pub total: u64,
}

impl Budget {
    pub fn consume(&mut self, slot: &str, tokens: u64) -> Result<()> {
        if self.total < tokens {
            return Err(Error::BudgetExhausted(slot.into()));
        }
        let slot_remaining = self.slots.get(slot).copied().unwrap_or(0);
        if slot_remaining < tokens {
            return Err(Error::BudgetExhausted(slot.into()));
        }
        self.slots.insert(slot.to_string(), slot_remaining - tokens);
        self.total -= tokens;
        Ok(())
    }
}
```

Cada fase pide su slot antes de iniciar:

```rust
budget.consume("sketches", estimated_tokens)?;
```

Si falla, la fase se aborta con `error='budget_exhausted'`.

---

## 33. Orden de `manifest.json` (final)

```json
{
  "schema_version": "v1",
  "run_id": "uuid7",
  "parent_run_id": null,
  "shared_brief_hash": null,
  "mode": "discovery",
  "created_at": "...",
  "started_at": "...",
  "ended_at": "...",
  "status": "completed",
  "client": {
    "cli_version": "0.4.0",
    "os": "linux",
    "arch": "x86_64"
  },
  "execution_policy": {
    "timeouts": { "sketch": 120, "phase": 0, "total": 0 },
    "parallelism": { "sketch": 4, "phase": 4, "extraction": 4, "max": 4 },
    "interactive": true,
    "router": "auto"
  },
  "budget": {
    "tokens_total": 0,
    "tokens_used": 1240000,
    "tokens_remaining": 0,
    "by_slot": {}
  },
  "provider": {
    "current": "minimax",
    "plan_id": "weekly",
    "api_key_ref": "env:MINIMAX_API_KEY"
  },
  "provider_changes": [],
  "models_used": {
    "minimax": { "calls": 200, "tokens": 800000, "errors": 0 }
  },
  "execution_history": {
    "phases": ["intake", "clarify", "sketch", ...],
    "current_phase": "deliver",
    "checkpoint_count": 1
  },
  "discovery": {
    "matrix_cardinality": 240,
    "categories": 12,
    "uncategorized": 47,
    "saturation_events": 12,
    "outliers_preserved": 8
  },
  "deliverables": {
    "final_dir": "final",
    "files": ["cat_01.md", "cat_02.md", "summary.md"]
  },
  "lineage_paths": {
    "relative": { "brief": "brief.json", "final": "final" },
    "absolute": { "brief": "/home/.../.runs/018f.../brief.json", "final": "/home/.../.runs/018f.../final" }
  },
  "hashes": {
    "manifest.json": "sha256:...",
    "brief.json": "sha256:..."
  },
  "warnings": [
    "timeout_total=0 means infinite"
  ]
}
```

---

## 34. Resiliencia en descubrimiento

Si una llamada LLM falla en `discovery`:

- Se reintenta 1 vez.
- Si vuelve a fallar, se salta ese sketch y se continúa.
- El sketch se registra como `error` en `calls` y se mueve a `final/uncategorized.md` con un pie "skipped".

Si todos los sketches fallan, el run termina con `status='failed'`.

---

## 35. Manejo del `Switch api-key`

```rust
pub fn resolve_api_key(spec: &str, env: &HashMap<String, String>) -> Result<String> {
    if let Some(var) = spec.strip_prefix("env:") {
        env.get(var).cloned().ok_or(Error::ApiKeyInvalid(var.into()))
    } else if let Some(path) = spec.strip_prefix("file:") {
        let s = fs::read_to_string(path)?;
        Ok(s.trim().to_string())
    } else {
        // interactive
        let key = dialoguer::Password::new()
            .with_prompt(format!("API key for {}", spec))
            .interact()?;
        Ok(key)
    }
}
```

---

## 36. `Moagan` entrypoint

```rust
// src/main.rs
fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load_config()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            match cli.cmd {
                Cmd::Run { mode, .. } => run::run(mode, cli.cmd).await,
                Cmd::Continue { .. } => continue_cmd::run(cli.cmd).await,
                Cmd::Resume { run_id } => resume::run(run_id).await,
                Cmd::Rerun { run_id, .. } => rerun::run(run_id, cli.cmd).await,
                Cmd::Inspect { run_id, .. } => inspect::run(run_id, cli.cmd).await,
                Cmd::Import { source_path } => import::run(source_path).await,
                Cmd::Telemetry { cmd } => telemetry_cmd::run(cmd).await,
            }
        })
}
```

---

## 37. Concreción de los puntos pendientes

| Punto de la propuesta | Implementación |
|---|---|
| `run_id = uuid_v7` | `ids::new_run_id()` |
| `lineage_paths` (relative + absolute) | `Manifest::lineage_paths()` |
| `parent_run_id`, `shared_brief_hash` | Campos en `runs` SQL y `manifest.json` |
| `redact_in_telemetry` | `RedactWriter` en cada escritura |
| `redact_patterns` default | Lista en §5.2 |
| `cleanup policy` | `retention::apply()` con `archive` o `delete` |
| `max_parallelism` | `Parallelism::new(max)` |
| `BudgetPlan` | `Budget` struct |
| `Phase 0` (intake) | `phases::intake` |
| `Phase 1` (clarify) | `phases::clarify` |
| `Phase 2` (route) | `phases::route` |
| `Phase 3` (decompose) | `phases::decompose` |
| `Phase 4` (sketch) | `phases::sketch_phase` |
| `Phase 5` (proposal) | `phases::proposal_phase` |
| `Phase 6` (gate) | `phases::gate` |
| `Phase 7` (validate) | `phases::validate` |
| `Phase 8` (critique) | `phases::critique` |
| `Phase 9` (repair) | `phases::repair` |
| `Phase 10` (judge) | `phases::judge` |
| `Phase 11` (rank) | `phases::rank` |
| `Phase 12` (cluster) | `phases::cluster_proposals` |
| `Phase 13` (synthesize) | `phases::synthesize` |
| `Phase 14` (deliver) | `phases::deliver` |
| `Phase 15` (refine) | `phases::refine` |
| Discovery matrix | `discovery::matrix` |
| Discovery tagger | `discovery::tagger` |
| Discovery clusterer | `discovery::clusterer` |
| Discovery contradictions | `discovery::contradiction` |
| Discovery facets | `discovery::facet` |
| Discovery extractor | `discovery::extractor` |
| Discovery integrator | `discovery::integrator` |
| Discovery uncategorized | `discovery::uncategorized` |
| Hash de input | `CallKey::hash` |
| LLM cache | `llm::cache` |
| Provider trait | `llm::provider::Provider` |
| Multi-provider | `ProviderPool` (formerly `ProviderRegistry`; renamed in v0.10) |
| Plan limits | `PlanTracker` |
| Hibernación | `phases::handle_pause` |
| Switch mid-run | `moagan continue --switch-provider` |
| API key interactive | `resolve_api_key` |
| API key env / file | `resolve_api_key` |
| Dashboard | `telemetry::dashboard` |
| Export | `telemetry::export` |
| Verify SHA256SUMS | `telemetry::verify` |
| `moagan telemetry list/summary/compare/provider/view/export/cleanup/config/verify` | `cli::telemetry_cmd` |
| `moagan run` | `cli::run` |
| `moagan continue` | `cli::continue_cmd` |
| `moagan resume` | `cli::resume` |
| `moagan rerun` | `cli::rerun` |
| `moagan inspect` | `cli::inspect` |
| `moagan import` | `cli::import` |
| Sandbox | `sandbox::Sandbox` |
| Cancellation | `tokio_util::sync::CancellationToken` |
| Checkpoints humanos | `checkpoint::human` |
| Iteración localizada | `phases::refine` |
| Stash de manifests | `Manifest::save` atómico |

---

## 38. Riesgos y mitigaciones identificadas

| Riesgo | Mitigación |
|---|---|
| Provider deprecate un endpoint | Cada provider encapsula su URL; un cambio de versión se hace en una sola constante. |
| Cargo test slow | `cargo test --no-fail-fast` con timeout global. |
| Disk full | `write_atomic` primero escribe a `tmp`, luego `rename`. Si `rename` falla, se detecta. |
| Schema migration rota | Cada migración es atómica (`BEGIN IMMEDIATE; <sql>; COMMIT; PRAGMA user_version=N;`). |
| User kills process mid-write | El `RedactWriter` usa `flush` por escritura. Si el OS trunca, el último `tmp` puede quedar, pero no corrompe el archivo final. |
| LLM produces infinite output | `max_tokens` por role. Si se excede, se trunca y se marca `truncated=1`. |
| Sandbox command runs CMD injection | `comma` y args se pasan como `&[&str]`, no se construye string. |
| Network leaks secrets | Redacción en `RedactWriter` antes de escribir. |
| Long discovery blocks UI | `indicatif::ProgressBar` por fase. |
| Concurrencia en SQLite | `r2d2` pool con `BEGIN IMMEDIATE` para escrituras críticas. |
| Cache poisoning | Verificación cruzada: si los `usage` reales difieren del cache, se sobrescribe. |
| UUID v7 collisions | Probabilidad negligible; collisions slot en retry. |

---

## 39. Restricciones que el siguiente modelo debe respetar

- **No usar SDKs de Anthropic** → `reqwest` + JSON manual.
- **Rust estable 1.97.1, edition 2024** → `#[derive(...)]` estables, `async fn in traits` no sin `async_trait`.
- **`async_trait` permitido** para el trait `Provider` (alternativa: native AFIT, pero limita GATs; el siguiente modelo puede migrar).
- **Sin assets externos** → SimHash en lugar de embeddings.
- **`.env.example` versionado** → en §11.3.
- **Cada decisión documentada con razón** → tabla en §0.5.

---

## 40. Resumen de prioridades para el siguiente modelo

1. **Empezar por `fs_layout.rs`, `config.rs`, `storage/sqlite.rs`, `ids.rs`**: base sobre la que se monta todo.
2. **Luego `llm/provider.rs` + `llm/mock.rs` + `llm/http.rs`**: el corazón de la aplicación.
3. **Después `redact/` + `telemetry/`**: la observabilidad y privacidad son transversales.
4. **Recién entonces `phases/`**: orquestación que consume lo anterior.
5. **Tests desde el inicio**: `cargo test` verde tras cada módulo.

Esto le da al siguiente modelo una guía clara: el orden de implementación está implícito en la dependencia entre módulos.
