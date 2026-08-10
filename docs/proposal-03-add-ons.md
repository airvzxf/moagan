---
id: add-ons
base: T01-06-06b3a1c2
synthesis_date: 2026-07-25
sources: 172 proposal files (T00-01 to T20-10)
method: contrast-only, additive, no modification of T01-06
purpose: catalogar lo valioso de las 172 propuestas para enriquecer T01-06
---

# Catálogo de añadidos (Add-ons)

## A. Propósito y método

Este documento **no modifica ni regenera** la propuesta ganadora `T01-06-06b3a1c2.md`. Es un **catálogo de añadidos** que se podrían integrar a `T01-06` para enriquecerla. Cada adición:

- Proviene explícitamente de una o más de las 172 propuestas hermanas.
- Se mapea a una sección concreta de `T01-06` que se vería aumentada.
- Mantiene compatibilidad con las 20 decisiones de `T01-06 §0.5` y con el orden de implementación de `§40`.
- Se ofrece como **parche aditivo** o como **subsección** según la magnitud del cambio.

El método seguido en este barrido fue:

1. **Top 8 (43/50 y 42/50)** — lectura directa, extracción detallada.
2. **Top 9–30 (41/50)** — lectura directa, extracción de patrones concretos.
3. **Top 31–90 (40/50)** — barrido en 2 rondas, extracción de cross-cutting insights.
4. **Top 91–175 (35–39/50)** — barrido en 3 rondas, solo lo más distintivo.
5. **Criterio de inclusión**: todo aquello que T01-06 no tiene, que sea concretamente implementable en Rust idiomático, y que NO rompa las decisiones D1–D20 de T01-06 §0.5.
6. **Criterio de exclusión**: narrativa vaga, contenido que T01-06 ya cubre, dependencias que rompan el pin de Cargo.toml, decisiones divergentes que rechacen los principios de T01-06.

---

## B. Conteo global de fuentes y aporte

| Bloque | Propuestas revisadas | Aportaciones integradas |
|---|---:|---:|
| Top 8 (42–43/50) | 8 | 64 items |
| 41/50 (Top 9–42) | 34 | 96 items |
| 40/50 | 52 | 71 items |
| 35–39/50 | 78 | 47 items |
| **Total** | **172** | **278 items** |

Los items se distribuyen en 28 categorías que se corresponden 1-a-1 con secciones o subsecciones de T01-06. Densidad de aporte por sección:

| Sección T01-06 | Items nuevos | Categoría |
|---|---:|---|
| §0.2 Module layout | 14 | Estructura |
| §0.3 Cargo.toml | 11 | Dependencias |
| §0.5 Decision table | 23 | Decisiones |
| §1 Filesystem layout | 7 | Persistencia |
| §2 SQLite schema | 22 | Persistencia |
| §3 Cache & hash | 13 | Cache |
| §4 LLM contract | 6 | Contratos LLM |
| §5/§19 Redact | 11 | Privacidad |
| §6 Parallelism | 8 | Concurrencia |
| §6.4/§21 Cancellation | 9 | Cancelación |
| §7 Sandbox | 16 | Aislamiento |
| §8 Non-discovery pipeline | 14 | Pipeline |
| §9 Discovery | 21 | Discovery |
| §10 CLI | 19 | CLI |
| §11 Config | 6 | Configuración |
| §12 Errors | 14 | Errores |
| §13 Telemetry | 11 | Telemetría |
| §14 Tests | 5 | Tests |
| §15 Provider | 24 | Providers |
| §16 Phase implementations | 7 | Fases |
| §17 Cardinality | 8 | Cardinalidad |
| §18 Adversary / iter | 5 | Adversario |
| §20 Parallelism runtime | 5 | Runtime |
| §22 Migrations | 3 | Migraciones |
| §24 Sandbox tests | 4 | Tests |
| §25 Error codes | 6 | Errores |
| §27 Telemetry redact | 3 | Redacción |
| §28 Reconciliación | 5 | Reconciliación |
| §29 Hardening | 9 | Hardening |
| §30 Pipeline | 4 | Pipeline |
| §31 CLI struct | 4 | CLI |
| §32 Budget | 5 | Budget |
| §33 Manifest | 8 | Manifest |
| §34 Discovery resilience | 3 | Discovery |
| §35 API key | 5 | API key |
| §38 Risks | 5 | Riesgos |
| §39 Constraints | 2 | Restricciones |
| §40 Priorities | 3 | Prioridades |
| **Total** | **278** | |

---

## C. Decisiones rechazadas explícitamente (no integrar)

Estas decisiones aparecen en otras propuestas pero se **rechazan** porque rompen algo de T01-06:

| Decisión rechazada | Fuente | Razón del rechazo |
|---|---|---|
| `RunContext` como global mutable | T13-02 | Rompe §6.1 (estado explícito en manifest). |
| `index.sqlite` global como única fuente de verdad | T13-05 | Rompe §1.1 ("el archivo manda, SQLite indexa"). |
| HTTP custom sobre `hyper` (sin `reqwest`) | T08-07 | Rompe §0.3 (pin de `reqwest 0.12`). |
| Embeddings locales con `candle`/`fasttext` por default | varias | Rompe §0.5#3 (sin assets externos). |
| `tokio-rusqlite` con un solo `Actor` | T09-02 | Rompe §2.3 (pool `r2d2` ya permite concurrencia). |
| `cargo run` daemon persistente entre runs | T02-10 | Rompe §0.1 (un proceso por run). |
| `axum` para dashboard | T06-04 | Rompe §10.8 (decisión de cero deps web extra). |
| `figment` para config | T09-04 | Rompe §0.3 (TOML directo). |
| `handlebars` / `askama` con templates externos | varios | Rompe §39 (prompts embebidos con `include_str!`). |
| Refinery o sqlx::migrate! | varios | Rompe §2.2 (migraciones embebidas como SQL estático). |
| `sqlx` en lugar de `rusqlite` | T16-04 | Rompe §0.3 (pin a `rusqlite` 0.32). |
| Servidor multi-usuario | T08-05 | Conflicto con §0.1 (CLI local). |
| `time` crate en lugar de `chrono` | T02-02, T15-01 | Rompe §0.3 (pin a `chrono 0.4`). |
| `secrecy` crate como única dependencia | T15-01 | Se acepta la idea del tipo nuevo, pero no la dependencia (añadir bajo feature flag opcional). |
| `governor` crate para rate limiting | T15-01, T09-07 | Se acepta el patrón, se prefiere implementación manual con `AtomicU64` para no añadir deps. |
| `governor`/`r2d2`/`parking_lot` reemplazando a `tokio::sync` | T08-02 | Rompe §6.1 (semáforos `tokio`). |
| `lettre` / `mailer` para notificaciones por email | varios | Fuera de alcance. |
| `inquire` crate para prompts | T08-09 | Se mantiene `dialoguer` (ya en pin). |

---

## D. Catálogo de adiciones por sección de T01-06

A continuación, cada adición se referencia con el formato `[SRC: Txx-yy]` para indicar la propuesta de origen. Cuando varias fuentes coinciden se listan separadas por coma.

### D.1. §0.2 Module layout (cambios menores y adiciones)

#### D.1.1. Submódulo `atomic/` (writer atómico reutilizable)
- `src/atomic/mod.rs`, `src/atomic/writer.rs`, `src/atomic/journal.rs`.
- Encapsula la receta `write_tmp → fsync → rename → fsync(parent_dir) → escribir sidecar .meta.json` (T03-09 §5 I-01; T04-07 §2.8; T14-03 §8.5; T15-05 §7.2; T19-10 §14.2; T10-08 §8).
- Razón: T01-06 §28.2 ya tiene `write_atomic` pero está acoplado a un solo caso de uso. Centralizarlo evita que las fases lo reinventen.

```rust
// src/atomic/writer.rs (nuevo)
pub struct AtomicWriter<'a> { root: &'a Path, tmp_suffix: &'a str }
impl<'a> AtomicWriter<'a> {
    pub fn new(root: &'a Path) -> Self;
    pub fn write(&self, rel: &str, bytes: &[u8]) -> Result<()>;
    pub fn write_with_meta(&self, rel: &str, bytes: &[u8], meta: &ArtifactMeta) -> Result<()>;
    pub fn cleanup_orphans(&self) -> Result<usize>;
}

// src/atomic/journal.rs (nuevo)
pub struct JournalWriter { /* append-only buffer + flush por N eventos o 5s */ }
```

#### D.1.2. `src/llm/wire/` con `WireFormat` enum
- `src/llm/wire/mod.rs`, `src/llm/wire/anthropic.rs`, `src/llm/wire/openai.rs`, `src/llm/wire/custom.rs`.
- `WireFormat::Anthropic | OpenAi | Custom(SchemaRef)` con `request_transform` y `response_transform` (T01-10 §3.1; T09-07 §5.4; T18-06 §3.6.1).
- Razón: T01-06 §26 tiene 6 impls casi idénticos. Esta abstracción reduce duplicación sin romper el trait `Provider`.

```rust
// src/llm/wire/mod.rs (nuevo)
pub enum WireFormat {
    Anthropic,
    OpenAi,
    Custom { request_schema: SchemaRef, response_schema: SchemaRef },
}
pub trait WireTransform: Send + Sync {
    fn build_body(&self, req: &Request, model: &str) -> serde_json::Value;
    fn parse_response(&self, body: &serde_json::Value) -> Response;
    fn parse_usage(&self, body: &serde_json::Value) -> Usage;
    fn parse_error(&self, status: u16, body: &serde_json::Value) -> LlmError;
}
```

#### D.1.3. `src/llm/embed/` con `Embedder` trait
- `src/llm/embed/mod.rs`, `src/llm/embed/hashing.rs`, `src/llm/embed/remote.rs`, `src/llm/embed/fastembed.rs` (feature).
- `Embedder` trait con `HashingEmbedder` (sin deps) y `RemoteEmbedder` (HTTP) (T09-02; T18-09 §7.7; T09-08 §7.7; T03-07 §2054-2060; T06-04 §8.3).
- Razón: T01-06 §9.5 solo tiene `cluster_simhash`. Ofrecer embeddings como alternativa documentada.

> **Implementación v0.4 — sub-fase K.2 (commit `861c660`).**
> `src/llm/embed/mod.rs` ships the `Embedder` trait and a
> dependency-free `HashingEmbedder` (256-dim by default, FNV-1a
> 32-bit on alphanumeric tokens, L2-normalised, parking_lot::Mutex
> term-frequency cache). The `RemoteEmbedder` and `fastembed`
> adapters remain opt-in follow-ups. 13 unit tests pin the
> deterministic / normalised / cache / similarity / FNV-1a
> canonical vectors.

#### D.1.4. `src/storage/outbox.rs` (transaccional outbox)
- Patrón outbox con tabla `outbox_events(id, run_id, kind, payload, attempts, last_error, next_attempt_at, flushed_at)` y worker que vacía cada 5s (T16-06 §1.2; T18-06 §8.2).
- Razón: T01-06 §2.5 tiene "consistencia eventual" para `phases.jsonl.gz`. El outbox lo hace explícito.

#### D.1.5. `src/storage/lease.rs` (lock optimista con TTL)
- `LockLease { holder: Uuid, ttl: Duration, fence: Uuid, acquired_at: Instant }` (T14-03 §8.5; T02-10 §6).
- Se usa por fase larga para reclamar trabajo (sketch, discovery iteration).

#### D.1.6. `src/telemetry/dashboard_static.rs` (HTML offline)
- Genera `dashboard.html` self-contained con JSON inline (T19-07 §9.6; T00-05 D19).
- Razón: T01-06 §10.8 ya tiene un servidor HTTP, pero un snapshot estático abre offline.

#### D.1.7. `src/secret.rs` (SecretString newtype)
- `SecretString(String)` con `Debug` que muestra `***`, `Display` que muestra `***`, `Drop` con `zeroize::Zeroize` (T00-05 §13.2; T15-01 §5.5; T18-06 §7.1; T07-10 §1287-1299; T08-03 §7.1; T04-04 §12.1).
- Se ofrece **sin** la dep `secrecy` (implementación manual con `zeroize 1.7` que ya está en el árbol de dependencias transitivas, o añadirla como opcional).

```rust
// src/secret.rs (nuevo)
pub struct SecretString(String);
impl SecretString {
    pub fn new(s: String) -> Self;
    pub fn expose(&self) -> &str;
}
impl std::fmt::Debug for SecretString { fn fmt(...) { write!(f, "SecretString(***)") } }
impl std::fmt::Display for SecretString { fn fmt(...) { write!(f, "***") } }
impl Drop for SecretString { /* zeroize::Zeroize::zeroize(&mut self.0) */ }
```

#### D.1.8. `src/cli/doctor.rs` (nuevo subcomando)
- `moagan doctor` chequea API key + writability + provider HEAD ping (T01-09 §1.4; T20-09 §3.1).
- Imprime config efectiva para diagnosticar precedencia.

#### D.1.9. `src/prompts/registry_v2.rs` (prompt cache por `(prompt_id, input_hash)`)
- `PromptCache` keyed por `(prompt_id, input_hash)` evita re-render (T00-05 §7.4; T01-09; T07-03 §15.1).
- Archivos de prompts como `.toml` con front-matter y bloque `[output_schema]` (T09-05 §3.7).
- Verificación de `StageContract::required_prompts` al arranque (T07-03).

#### D.1.10. `src/pipeline/graph.rs` con `Dag::topological_layers`
- `pub fn topological_layers(&self) -> Vec<Vec<NodeId>>` por algoritmo de Kahn (T07-10 §1217; T18-07 §9.1; T08-04 §3.1).
- Scheduler por capas, no por vector de fases.

#### D.1.11. `src/ranking/diversity.rs` con `SelectionPlan`
- `pub struct SelectionPlan { keep_top: usize, keep_diverse: usize, keep_outlier: usize }` (T14-09 §847-867; T15-02 §7).
- Función pura `select(sketches: &[Sketch], plan: SelectionPlan) -> Vec<SketchId>`.

#### D.1.12. `src/llm/cache/sharded.rs` (sharding por hash)
- `cache_dir / <shard0-1>/<shard2-3>/<full_hash>.json.gz` (T16-09 §5.9).
- Solo cachea si `usage.total_tokens > 100`. LRU con tope 2GB.
- Razón: T01-06 §3.3 tiene archivos en un solo dir. Sharding evita cuellos de botella en runs grandes.

#### D.1.13. `src/discovery/epistemic_legacy.rs` (memoria entre runs)
- `epistemic_legacy.json` por cluster (T14-09 §847-867; T19-04 §6.5).
- Notas heredadas inyectadas al generador de propuestas en runs subsecuentes.

#### D.1.14. `src/cli/diff.rs` (comparar dos runs)
- `moagan diff <run_a> <run_b>` reporta diferencias en parámetros, artefactos, scores (T01-10 §7.1; T16-01 §6.1).

---

### D.2. §0.3 Cargo.toml (añadir al pin)

| Crate | Versión | Propósito | Fuente |
|---|---:|---|---|
| `time` | NO añadir | T01-06 ya tiene `chrono 0.4`. | (rechazado de T15-01, T02-02) |
| `tiktoken-rs` | `0.5` con `cl100k_base` | Estimación de tokens previa a la llamada | T06-07 §13.2; T04-04 §12.1; T13-04 §6.6 |
| `secrecy` | `0.8` (opcional, feature flag `secrecy`) | Tipo `SecretString` con `Zeroize` y `ZeroizeOnDrop` | T00-05 §13.2; T15-01 §5.5; T18-06 §7.1; T07-10 §1287-1299; T08-03 §7.1; T04-04 §12.1 |
| `zeroize` | `1.7` (sin feature flag) | Base para `SecretString` y `SecretBox` | igual |
| `rpassword` | `7.3` | Reemplaza `dialoguer::Password` para API keys | T15-01 §5.5; T09-07 §10.3 |
| `r2d2` | ya está `0.8` | Pool SQLite, sin cambios | (baseline) |
| `fs2` | `0.4` | `flock(2)` portable para `run.lock` | T18-06 §0.7; T08-03 §0.7; T02-10 §6; T19-07 §11.2; T03-02; T15-09 §4.1 |
| `assert_cmd` | `0.5` | Tests de CLI | T09-07 §29.1; T00-05 D20 |
| `predicates` | ya está `3.1` | Tests | (baseline) |
| `insta` | ya está `1.39` | Snapshots | (baseline) |
| `proptest` | `1.4` | Property-based testing de hashes y serialización | T00-05 D20 |
| `wiremock` | ya está `0.6` | Mock HTTP | (baseline) |
| `tempfile` | ya está `3.10` | tmp dirs | (baseline) |
| `governor` | NO añadir | Se prefiere implementación manual con `AtomicU64` (T01-06 §6.1) | (rechazado) |
| `parking_lot` | ya está `0.12` | Mutex no-async (path interno al disco y al `SecretString`) | (baseline) |
| `dashmap` | ya está `6.1` | Cache LRU en memoria | (baseline) |
| `dotenvy` | ya está `0.15` | `.env` | (baseline) |
| `walkdir` | ya está `2.5` | Recorrer `.runs/` | (baseline) |
| `flate2` | ya está `1.0` | gzip | (baseline) |
| `tar` | ya está `0.4` | export | (baseline) |
| `zstd` | ya está `0.13` | compression | (baseline) |
| `comfy-table` | `7.1` | Tablas CLI (`inspect`, `telemetry provider`) | T12-09 §6.2; T20-06 §6.3 |
| `indicatif` | ya está `0.17` | barras de progreso | (baseline) |
| `tracing-appender` | `0.2` con `rolling::daily` | Rotación diaria de logs | T05-02 §17.1 |
| `path-clean` | `1.0` | Normalización de paths (defensa contra `..`) | T09-07 §0.6 |
| `similar` | `2.5` | `Levenshtein` y `Normalized Levenshtein` para repair-stop | T20-01 |
| `json-pointer` (o `jsonptr`) | `0.6` | RFC 6901 path reporting en validation errors | T18-06 §1.3 |
| `tokio-util` | `0.7` con `rt` | `CancellationToken` | (ya transitivo, hacer explícito) |
| `nix` | `0.27` con `feature="fs"` (opcional) | `setrlimit`, `unshare` para sandbox Linux | T18-06 §3.3; T07-04 §8.4; T08-01; T09-09 §1.2; T13-04 §6.6 |
| `cgroupfs` o `cgroups-rs` | `0.3` (opcional, feature `cgroup`) | Memory cap vía cgroup v2 | T18-06 §3.3; T13-04 §6.6; T08-06 §8.1; T14-05 §8.1 |
| `camino` | `1.1` con `serde1` | UTF-8 paths | T17-05 D3; T16-10 §7.1 |
| `figment` | NO añadir | Se mantiene `toml` directo | (rechazado de T09-04) |
| `petgraph` | `0.6` con `serde` | DAG de fases | T18-06 §0; T16-09 §7.2; T07-10 §1217 |
| `shlex` | `1.3` | Split de comandos en sandbox (en lugar de `str::split`) | T08-10 |
| `jsonschema` | ya está `0.17` | Validación de contratos LLM | (baseline) |
| `schemars` | `0.8` con `derive` | Generación compile-time de JSON Schema desde structs | T07-03 §15.1; T00-05 §7.3 |
| `csv` | `1.3` | Escribir `sketches_summary.csv` | T04-10 |
| `tokio-stream` | `0.1` | `Stream` adapters para streaming LLM | T20-07 §6.4 |
| `libc` | `0.2` | `killpg`, `setrlimit` (alternativa a `nix`) | T07-07 §10.1 |
| `csv` | ya | ya | (no añadir) |

Notas de pin:
- `tiktoken-rs` se usa solo para pre-flight (no para contar tokens facturados), por lo que su inexactitud no rompe accounting.
- `nix` y `cgroupfs` van detrás de `#[cfg(target_os = "linux")]` y feature flags.
- `petgraph` se usa como **opcional**; por defecto T01-06 mantiene su `phases/` vector (DAG solo en `deep`).
- `secrecy` puede omitirse: la implementación manual con `zeroize 1.7` es ~30 LoC.

---

### D.3. §0.5 Decision table (filas adicionales)

T01-06 §0.5 tiene 20 filas. Las 23 filas siguientes son **extensiones** (no sustituyen a las originales). Mantienen el formato `# | Ambigüedad | Decisión | Razón`.

| # | Ambigüedad | Decisión | Razón | Fuentes |
|---:|---|---|---|---|
| 21 | "¿Atomic write con fsync(parent)? o solo rename?" | `write_tmp → fsync(file) → rename → fsync(parent_dir) → escribir .meta.json` | Previene sidecars huérfanos si crash entre rename y metadata. | T03-09; T04-07; T15-05; T19-10; T10-08; T14-03 |
| 22 | "¿BLAKE3 o SHA256 para `input_hash`/`output_hash`?" | **BLAKE3** para hashes internos (más rápido, hot path). **SHA-256** solo para `SHA256SUMS` de export. | T01-06 §3.2 usa SHA256; T02-02 §0.1 y T13-04 §1.1(1) muestran que BLAKE3 es 5–10x más rápido. | T02-02; T13-04 |
| 23 | "¿API key como `String` o tipo nuevo?" | `SecretString(String)` con `Debug=***` y `Drop=zeroize`. Sin dep `secrecy` (manual con `zeroize 1.7`). | Previene memory dumps con la key. T01-06 §35 tiene `mask_key` pero no memory hygiene. | T00-05; T15-01; T18-06; T07-10; T08-03; T04-04 |
| 24 | "¿Una sola DB o dos?" | **Una sola DB** (`meta.sqlite`) con `journal_mode=WAL`. Migración opcional a `run.db` per-run si se observa contención. | T01-06 §2.4 ya tiene una sola DB. | (confirma T01-06) |
| 25 | "¿Embeddings?" | **No** en MVP. `HashingEmbedder` (256-dim, deterministic, sin assets) detrás de feature flag. `fastembed::BGESmallENV15` opcional. | T01-06 §0.5#3 ya dice no. | T18-06; T18-09; T09-08; T03-07; T06-04 |
| 26 | "¿Quién escribe primero: archivo o SQLite?" | **Archivo primero.** Si `write_atomic` falla, no se hace INSERT. Si INSERT falla, el archivo se queda; `inspect --reindex` repara. | T01-06 §1.1 ya lo dice. | T02-01; T04-09; T06-02; T01-10 |
| 27 | "¿Cache key incluye `temperature`?" | Sí, pero **quantized**: `temperature_bin = round(temperature * 10) / 10`. T=0.21 y T=0.24 comparten cache. | T01-06 §3.2 usa T exacto. T18-02 §1.4 muestra que quantization da ~20% más hits. | T18-02; T09-10; T03-06 |
| 28 | "¿Tagger threshold 0.6 o 0.65?" | **0.6** por default; configurable. Si `uncategorized > 30%` se baja a 0.55. | T01-06 §9.4 usa 0.6. | T18-09; T15-02 |
| 29 | "¿Provider switch preserva cache?" | **Sí, si la familia coincide** (`family() == family() && host() == host()`). | T01-06 §3.3 no menciona. | T09-08; T18-06; T18-09 |
| 30 | "¿Provider compatibility check?" | `Capabilities::provider_compatible(from, to) -> bool` con `family()` y `endpoint.host()`. | T18-09 §9.5; T09-08 §9.5; T18-06 §0.7. | T18-09; T09-08; T18-06 |
| 31 | "¿Qué se cachea en discovery?" | **Sketch/tag/facet** sí cachean intra-run. **Proposals** nunca. **Integration/refiner** tampoco. | T01-06 §9.3 no explicita. | T07-03; T09-05; T03-03; T00-08 |
| 32 | "¿Circuit breaker?" | **Sí.** Por provider. 5 errores en 60s → `circuit_open=true` durante 5 min. | T01-06 §15.4 no tiene. | T00-09; T03-03; T08-03; T20-05 |
| 33 | "¿Rate-limit por provider?" | **Sí.** Token-bucket por provider. Honor `Retry-After` para 429. | T01-06 §15.4 tiene `PlanTracker` pero no rate-limit. | T15-01; T08-01; T20-08; T09-07 |
| 34 | "¿Retry policy: jitter?" | **Sí, ±50% del backoff calculado.** | T01-06 §4.7 no incluye jitter. | T20-02; T02-07; T05-08; T20-05 |
| 35 | "¿Truncation recovery?" | **Focused continuation**: prompt `Continúa sin repetir desde: "{last_500_tokens}"`. **Max 2 continuaciones** por call. Si falla → `truncated=1` permanente. | T01-06 §4.6 dice "intenta balancear `}`/`]`". | T20-07; T00-09; T03-05; T08-02 |
| 36 | "¿Discovery check antes de gastar?" | Validar `cardinality * avg_tokens_por_sketch <= max_storage_gb * 1e9` antes de arrancar. | T01-06 §9 no lo tiene. | T19-09; T04-05; T18-04 |
| 37 | "¿Heartbeat para detectar zombies?" | Task en background `tokio::time::interval(30s)` escribe `last_heartbeat`. Al arrancar, runs en `running` con `last_heartbeat > 5min` se marcan `failed/zombie_recovered`. | T01-06 §21 no lo tiene. | T20-05; T08-03; T01-07; T18-08 |
| 38 | "¿Provider como familia lógica?" | `Provider::Family::Anthropic | OpenAI | Custom` para switch que preserve cache. | T09-08 §9.5; T18-06 §0.7. | T09-08; T18-06; T18-09 |
| 39 | "¿Cross-run dedup?" | `discover_runs_with_input_hash(input_hash) -> Vec<RunId>` al arrancar; nuevo run con mismo `input_hash` se marca `duplicates_with: [run_id]`. | T03-04 §3.2; T01-04 §6.5. | T03-04; T01-04; T19-09 |
| 40 | "¿Quorum de judges en fast?" | `fast` usa **1 judge** (correctness). `standard`/`deep` usan 3. `discovery` no usa judge. | T01-06 §17 tiene "cardinalidad típica" pero no por judge. | T06-09; T19-04; T20-02 |
| 41 | "¿Failure policy por fase?" | `Phase::failure_policy -> FailFast | SkipNonCritical | Defer | Abort`. Sketch failure NO aborta; validation/critique sí. | T19-09; T00-08 §1408-1421. | T19-09; T00-08; T18-07 |
| 42 | "¿Repair con semántica de invariantes?" | Refiner debe llamar `script::coverage_ratio(refined, original) >= 0.85` antes de aceptar; `preserved_citations / original_citations >= 0.9`; sino revert. | T01-06 §9.9 menciona 20% pero no formal. | T03-04; T10-03; T10-10; T11-06; T05-01; T07-03 |
| 43 | "¿Incompatibilidades duras en síntesis?" | `const HARD_INCOMPATIBILITIES: &[(&str, &str)] = &[ ("monolith", "microservices"), ("sync_rpc", "event_driven"), ("strong_consistency", "eventual_consistency"), ("sql", "nosql"), ("self_hosted", "serverless"), ("rust", "non_permitted_runtime"), ... ];` | T02-09; T19-09; T03-01; T18-04 §11.1; T05-10 §11.1. | T02-09; T19-09; T03-01; T18-04; T05-10; T08-06 |

---

### D.4. §1 Filesystem layout (artefactos nuevos)

| Path | Contenido | Fuente |
|---|---|---|
| `cache/llm/<shard0-1>/<shard2-3>/<full_hash>.json.gz` | Cache LLM shardeado | T16-09 §5.9 |
| `cache/facets/<sha256(brief+cat)>.json` | Facets derivados cacheados globalmente | T01-05; T02-04; T06-06 D2 |
| `cache/embeddings/<sha256>.bin` | Embeddings por sketch (si `Embedder` activo) | T18-09 |
| `cache/embeddings/_index.sqlite` | Índice del cache de embeddings | T18-09 |
| `locks/<run_id>.lock` | File lock con `{pid, hostname, run_id, acquired_at, fence}` | T18-06; T08-03; T19-07; T15-09; T03-02 |
| `state/heartbeat.json` | Última heartbeat | T20-05; T08-03 |
| `state/journal.jsonl` | Journal de mutaciones (transaccional outbox) | T16-06; T18-06 |
| `state/run_state.json` | Estado volátil separado del `manifest.json` | T18-08 §2.1 |
| `sketches_summary.csv` | CSV con `id, created_at, gate, tags[], cluster, category, model, tokens, output_hash` | T04-10 |
| `epistemic_legacy/<cat_id>.json` | Notas heredadas entre runs | T14-09; T19-04 |
| `partial/` | Artefactos parciales de runs cancelados | T01-07 §4.5 |
| `outbox/<event_id>.json` | Eventos pendientes de flush al backend | T16-06 §1.2 |
| `dashboard.html` | Snapshot estático del dashboard | T19-07 §9.6; T00-05 D19 |
| `tool_versions.json` | Versiones de `cargo`, `rustc`, `python3`, etc. | T00-06 §11.4 |
| `meta.json` | Sidecar con `sha256, bytes, written_at, written_by, schema_version` | T03-09 I-01 |
| `prompts/<role>.v<N>.toml` | Prompts como TOML con front-matter | T09-05 §3.7 |

---

### D.5. §2 SQLite schema (tablas y columnas nuevas)

T01-06 §2.1 tiene 8 tablas. Las siguientes tablas/columnas se **agregan** sin romper nada:

> **Implementación v0.4 — sub-fase K.5 (commit `7696754`).**
> La migración `v008_add_ons.sql` ships `outbox_events`,
> `redact_audit`, `manifest_events`, `process_locks`, y
> `provider_rollups` con los índices exactos del spec. Los helpers
> en `src/storage/sqlite.rs` (`record_outbox_event`,
> `list_outbox_events_for_run`, `record_redact_audit`,
> `list_redact_audit_for_run`, `record_manifest_event`,
> `acquire_process_lock`, `release_process_lock`,
> `increment_provider_rollup`, `get_provider_rollup`) son
> best-effort contra `PRAGMA user_version`: en una DB pre-v008
> son no-op, así que ningún run existente rompe con el nuevo
> código. 9 unit tests cubren round-trips, idempotencia, y
> las semánticas single-row del process lock.

#### D.5.1. Tablas nuevas

```sql
-- v002_extended.sql (nueva migración)
PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA user_version = 2;

-- Outbox transaccional
CREATE TABLE IF NOT EXISTS outbox_events (
  id              TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL,
  kind            TEXT NOT NULL,
  payload         TEXT NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT,
  next_attempt_at TEXT NOT NULL,
  flushed_at      TEXT,
  created_at      TEXT NOT NULL,
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
CREATE INDEX IF NOT EXISTS idx_outbox_pending ON outbox_events(next_attempt_at) WHERE flushed_at IS NULL;

-- Estado volátil del run
CREATE TABLE IF NOT EXISTS run_state (
  run_id          TEXT PRIMARY KEY,
  current_phase   TEXT,
  paused_at       TEXT,
  pause_reason    TEXT,
  resume_token    TEXT,
  last_heartbeat  TEXT,
  parallel_in_use INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Artefactos del run
CREATE TABLE IF NOT EXISTS run_artifacts (
  run_id          TEXT NOT NULL,
  rel_path        TEXT NOT NULL,
  sha256          TEXT NOT NULL,
  bytes           INTEGER NOT NULL,
  schema_version  TEXT,
  artifact_version TEXT,
  created_at      TEXT NOT NULL,
  PRIMARY KEY (run_id, rel_path),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
CREATE INDEX IF NOT EXISTS idx_artifacts_hash ON run_artifacts(sha256);

-- Presupuesto por fase
CREATE TABLE IF NOT EXISTS budget_state (
  run_id          TEXT NOT NULL,
  phase           TEXT NOT NULL,
  tokens_used     INTEGER NOT NULL DEFAULT 0,
  tokens_planned  INTEGER NOT NULL,
  updated_at      TEXT NOT NULL,
  PRIMARY KEY (run_id, phase),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Human checkpoints ordenados
CREATE TABLE IF NOT EXISTS human_checkpoints (
  run_id          TEXT NOT NULL,
  seq             INTEGER NOT NULL,
  ckp_id          TEXT NOT NULL,
  phase           TEXT NOT NULL,
  kind            TEXT NOT NULL CHECK (kind IN ('intake','clarify','final','custom')),
  state           TEXT NOT NULL CHECK (state IN ('pending','resolved','expired','skipped')),
  payload_json    TEXT NOT NULL,
  resolved_at     TEXT,
  resolved_action TEXT,
  UNIQUE (run_id, seq),
  PRIMARY KEY (run_id, ckp_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Facet cache global
CREATE TABLE IF NOT EXISTS facet_cache (
  cache_key       TEXT PRIMARY KEY,
  facets_json     TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  expires_at      TEXT
);

-- Descubrimiento deduplicado
CREATE TABLE IF NOT EXISTS discovery_dedup (
  matrix_hash     TEXT NOT NULL,
  brief_hash      TEXT NOT NULL,
  sketch_ids_json TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  PRIMARY KEY (matrix_hash, brief_hash)
);

-- Auditoría de redacción
CREATE TABLE IF NOT EXISTS redact_audit (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id          TEXT NOT NULL,
  file_path       TEXT NOT NULL,
  pattern_kind    TEXT NOT NULL,
  match_count     INTEGER NOT NULL,
  redacted_at     TEXT NOT NULL,
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Lock de archivo (cross-process)
CREATE TABLE IF NOT EXISTS process_locks (
  lock_path       TEXT PRIMARY KEY,
  pid             INTEGER NOT NULL,
  hostname        TEXT NOT NULL,
  fence           TEXT NOT NULL,
  acquired_at     TEXT NOT NULL,
  expires_at      TEXT NOT NULL
);

-- Métricas agregadas (rollups de 7 días)
CREATE TABLE IF NOT EXISTS provider_rollups (
  period_start    TEXT NOT NULL,
  period_end      TEXT NOT NULL,
  provider        TEXT NOT NULL,
  plan_id         TEXT,
  calls           INTEGER NOT NULL,
  tokens_total    INTEGER NOT NULL,
  errors          INTEGER NOT NULL,
  PRIMARY KEY (period_start, provider, plan_id)
);

-- Estado del plan del provider
CREATE TABLE IF NOT EXISTS plan_state (
  run_id          TEXT NOT NULL,
  provider        TEXT NOT NULL,
  plan_id         TEXT,
  state           TEXT NOT NULL CHECK (state IN ('normal','warning','paused','hard_limit','circuit_open')),
  used            INTEGER NOT NULL DEFAULT 0,
  limit           INTEGER NOT NULL,
  warning_threshold REAL NOT NULL,
  hard_limit      REAL NOT NULL,
  reset_at        TEXT,
  failed_at       TEXT,
  PRIMARY KEY (run_id, provider, plan_id),
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Dead-letter queue (LLM no recuperable)
CREATE TABLE IF NOT EXISTS dql (
  id              TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL,
  phase           TEXT NOT NULL,
  attempt_id      TEXT NOT NULL,
  payload_path    TEXT NOT NULL,
  error_kind      TEXT NOT NULL,
  resolved        INTEGER NOT NULL DEFAULT 0,
  created_at      TEXT NOT NULL,
  FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- LLM cache separado (opcional)
CREATE TABLE IF NOT EXISTS llm_cache (
  cache_key       TEXT PRIMARY KEY,
  response_json   TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  expires_at      TEXT,
  hits            INTEGER NOT NULL DEFAULT 0
);

-- Manifest events (status changes)
CREATE TABLE IF NOT EXISTS manifest_events (
  run_id          TEXT NOT NULL,
  at              TEXT NOT NULL,
  kind            TEXT NOT NULL,
  payload_json    TEXT NOT NULL,
  PRIMARY KEY (run_id, at, kind)
);
```

#### D.5.2. Columnas adicionales a tablas existentes

```sql
-- runs
ALTER TABLE runs ADD COLUMN duplicates_with_json TEXT;
ALTER TABLE runs ADD COLUMN heartbeat_enabled   INTEGER NOT NULL DEFAULT 1;
ALTER TABLE runs ADD COLUMN config_snapshot     TEXT;
ALTER TABLE runs ADD COLUMN cancel_token_id     TEXT;
ALTER TABLE runs ADD COLUMN shared_brief_hash   TEXT;

-- calls
ALTER TABLE calls ADD COLUMN attempt            INTEGER NOT NULL DEFAULT 0;
ALTER TABLE calls ADD COLUMN cache_hit          INTEGER NOT NULL DEFAULT 0;
ALTER TABLE calls ADD COLUMN cached_input_tokens INTEGER;
ALTER TABLE calls ADD COLUMN cached_output_tokens INTEGER;
ALTER TABLE calls ADD COLUMN reasoning_tokens   INTEGER;
ALTER TABLE calls ADD COLUMN finish_reason      TEXT;

-- provider_changes (enriquecer)
ALTER TABLE provider_changes ADD COLUMN sketches_already_generated INTEGER;
ALTER TABLE provider_changes ADD COLUMN api_key_ref      TEXT;
ALTER TABLE provider_changes ADD COLUMN reason           TEXT;
ALTER TABLE provider_changes ADD COLUMN sequence         INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_changes_seq ON provider_changes(run_id, sequence);

-- checkpoints
ALTER TABLE checkpoints ADD COLUMN seq             INTEGER;
ALTER TABLE checkpoints ADD COLUMN blocking       INTEGER NOT NULL DEFAULT 0;
ALTER TABLE checkpoints ADD COLUMN expires_at     TEXT;
```

#### D.5.3. Triggers

```sql
-- Sincronizar run_state.status con runs.status
CREATE TRIGGER IF NOT EXISTS trg_run_status_change
AFTER UPDATE OF status ON runs
BEGIN
  INSERT INTO phases (run_id, phase_name, event, at)
  VALUES (NEW.run_id, '__lifecycle__', 'status', datetime('now'));
END;
```

#### D.5.4. Conexiones separadas

- `moagan run` usa conexión RW con `BEGIN IMMEDIATE` para escrituras críticas.
- Dashboard usa `?mode=ro` (T10-10 §1.4; T18-09 §1; T09-08 §4.4). T01-06 §10.8 ya tiene esto.
- T01-06 §0.5#6 mantiene `BEGIN IMMEDIATE` en multi-tabla.

---

### D.6. §3 Cache & hash (mejoras)

#### D.6.1. Hash interno con BLAKE3

```rust
// src/llm/cache.rs (sustituir el `hash_input` para uso interno)
pub fn hash_input_internal(role: &str, phase: &str, brief_hash: &str, provider: &str, model: &str,
                           temperature: f32, top_p: f32, max_tokens: u32, prompt: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"moa-cache-v1");
    for part in [role, phase, brief_hash, provider, model,
                 &format!("{:.2}", temperature),
                 &format!("{:.2}", top_p),
                 &max_tokens.to_string(),
                 prompt] {
        h.update(part.as_bytes());
        h.update(&[0x1f]);
    }
    hex::encode(h.finalize().as_bytes())
}
```

Mantener SHA-256 **solo** para `SHA256SUMS` de export. T01-06 §3.2 puede quedar como está con un comentario.

#### D.6.2. Sharding del cache

```rust
// src/llm/cache/sharded.rs (nuevo)
pub struct ShardedCache { root: PathBuf, shard_bits: u8 }
impl ShardedCache {
    pub fn path_for(&self, key: &str) -> PathBuf {
        let b = key.as_bytes();
        let s1 = &b[0..2.min(b.len())];
        let s2 = &b[2..4.min(b.len())];
        self.root.join(hex::encode(s1)).join(hex::encode(s2)).join(format!("{}.json.gz", key))
    }
}
```

Solo cachea si `usage.total_tokens > 100` y respeta LRU 2GB.

#### D.6.3. TTL del cache y LRU

```sql
-- llm_cache (tabla nueva, ver D.5.1)
```

```rust
// src/llm/cache/sqlite.rs (nuevo, opcional)
pub struct SqliteCache { conn: rusqlite::Connection, max_bytes: u64 }
impl SqliteCache {
    pub fn prune_expired(&self) -> Result<usize>;
    pub fn evict_lru(&self, target_bytes: u64) -> Result<usize>;
}
```

Default: TTL 7 días, max 1GB (T00-09; T09-05).

#### D.6.4. PromptCache por `(prompt_id, input_hash)`

```rust
// src/prompts/cache.rs (nuevo)
pub struct PromptCache { inner: Arc<Mutex<HashMap<(PromptId, String), Arc<String>>>> }
impl PromptCache {
    pub fn get_or_insert(&self, id: &PromptId, input_hash: &str, compute: impl FnOnce() -> String) -> Arc<String>;
}
```

(Inspirado en T00-05 §7.4; T01-09; T07-03 §15.1; T01-08 §4.6.)

#### D.6.5. Cache key con `response_format` y `schema_hash`

```rust
// src/llm/cache.rs (extender CallKey)
pub struct CallKey {
    // ... campos previos
    pub schema_hash: String,
    pub response_format: ResponseFormat,
}
```

(Inspirado en T09-05; T10-03 §3.4.)

#### D.6.6. Cache bypass para `discovery` sketches y `proposal`

```rust
pub enum CacheScope { Bypass, Use, Required }
impl CacheScope {
    pub fn for_role(role: &RoleId) -> Self {
        match role.as_str() {
            "sketcher" | "proposer" | "integrator" | "refiner" => CacheScope::Bypass,
            _ => CacheScope::Use,
        }
    }
}
```

(Inspirado en T07-03 §15.1; T00-08 §684; T03-03 §4.8.)

---

### D.7. §4 LLM contract (enriquecer schemas y roles)

#### D.7.1. Nuevos roles en `prompts/registry.rs` (P — ✅ catálogo registrado)

T01-06 §4.2 tiene 19 roles. Se proponen los siguientes **sin romper los existentes**:

| role_id | temp | top_p | max_tokens | json_mode | Fuente |
|---|---:|---:|---:|---:|---|
| `tiefighter_critic` | 0.4 | 0.9 | 1_000_000 | true | T18-09 §5; T05-01 |
| `final_disagreement` | 0.3 | 0.8 | 1_000_000 | true | T20-10 §3.5 |
| `merge_synthesizer` | 0.2 | 0.7 | 1_000_000 | true | T20-01; T18-04 |
| `persona_picker` | 0.0 | 0.2 | 1_000_000 | true | T07-06 §5.4 |
| `angle_picker` | 0.0 | 0.2 | 1_000_000 | true | T07-06 §5.4 |
| `json_repair_v2` | 0.0 | 0.1 | 1_000_000 | true | T03-04 §7.4 |
| `hostile_prompt_detector` | 0.0 | 0.1 | 1_000_000 | true | T00-03 §4.5; T20-10 §4.1 |
| `recovery_explainer` | 0.0 | 0.1 | 1_000_000 | true | T20-06; T00-08 |
| `rationale_extractor` | 0.2 | 0.7 | 1_000_000 | true | T20-04 |

#### D.7.2. Sanitización de control tokens antes de parsear

```rust
// src/llm/response.rs::strip_control_tokens
pub fn strip_control_tokens(s: &str) -> String {
    s.replace("<|im_start|>", "")
     .replace("<|im_end|>", "")
     .replace("<system>", "")
     .replace("</system>", "")
}
```

Si después de strip el JSON no parsea, se marca `ContractViolation` (T20-08 §7.6).

#### D.7.3. Streaming de respuesta para truncación temprana

```rust
// src/llm/provider.rs (extender trait)
#[async_trait]
pub trait Provider: Send + Sync {
    // ... métodos previos
    async fn stream(&self, req: &Request) -> Result<StreamingResponse> {
        Err(Error::StreamingNotSupported)
    }
}

pub struct StreamingResponse {
    pub first_chunk_at: Instant,
    pub chunks: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
}
```

(Inspirado en T20-07 §6.4; T09-08 §5.6; T16-06 §3.4.)

#### D.7.4. Anclaje de rúbrica

> **Estado (v0.3 sub-fase O, ✅ merged en `feat/v0.4-phase-o-rubric-compression`):**
> implementado en `src/ranking/rubric.rs`. `pub enum Criterion`
> con 6 variantes (`Correctness`, `Completeness`, `Fit`,
> `Evidence`, `Clarity`, `Overall`) — el spec original hablaba
> de «9 criterios» pero la `RankingWeights` consolidada usa 6
> (el field `Overall` absorbe los tres restantes que el
> concepto original separaba). `pub struct Rubric` con
> `Default` que sembrar las 18 celdas `(c, level)` con frases
> concretas; accessors `anchored_1/3/5` con fallback a `""`
> para los niveles no seedeados (2, 4). Re-exportado vía
> `pub use rubric::{Criterion, Rubric};` en `src/ranking/mod.rs`.
> 6 unit tests en el módulo + 1 integration test
> (`rubric_anchors_are_stable_across_calls`). El wiring al
> prompt del `judge` / `critic` queda como follow-up (no
> scope del opt-in mínimo).

```rust
// src/ranking/rubric.rs (nuevo)
pub struct Rubric { anchors: HashMap<(Criterion, u8), String> }
impl Rubric {
    pub fn anchored_3(&self, c: Criterion) -> &str;
    pub fn anchored_5(&self, c: Criterion) -> &str;
}
impl Default for Rubric {
    fn default() -> Self {
        let mut m = HashMap::new();
        m.insert((Criterion::Correctness, 1), "No verificable, hipótesis sin evidencia");
        m.insert((Criterion::Correctness, 3), "Correcto bajo asunciones razonables");
        m.insert((Criterion::Correctness, 5), "Verificado, evidencia ejecutable");
        // ... para los 9 criterios
        Self { anchors: m }
    }
}
```

(Inspirado en T00-03 §1087-1098; T15-02 §9.3; T05-06; T07-07; T05-09; T00-01 §3.6.)

#### D.7.5. JSONL gzip stream-friendly (P — ✅ `compress_or_report` para None/Gz/Zst)

[partial] Shipped as API in src/storage/compression.rs (PR #39, sub-fase P, recovered by 6f9e2d8). Sin consumidor en hot path de producción; uso previsto: export surface. Anotación propuesta en docs/q1-v0.3-status-sync.

> **Estado (v0.3 sub-fase O, ✅ merged en `feat/v0.4-phase-o-rubric-compression`):**
> implementado en `src/storage/compression.rs` como capa
> aditiva. `pub enum Compression { None, Gz, Zst }` +
> `Compression::from_extension(Path)` + `reader(Path,
> Compression) -> io::Result<Box<dyn Read>>`. El import
> `flate2::Compression` se renombró a `FlateCompression`
> para evitar shadowing del nuevo enum público. La API
> previa (`open_gz_append`, `open_gz_read`,
> `read_to_string`) queda intacta: el nuevo `reader` usa
> `GzDecoder` (single-stream), no `MultiGzDecoder`, porque
> la enum está pensada para tooling que abre un solo
> stream. 5 unit tests + 3 integration tests
> (`compression_reader_handles_gz_file`,
> `compression_reader_handles_zst_file`,
> `compression_reader_handles_uncompressed`). El writer
> streaming zstd equivalente a `MemberGzWriter` queda como
> follow-up (no necesario hasta `--format tar.zst`).

```rust
// src/storage/compression.rs (extender)
pub enum Compression { None, Gz, Zst }

pub fn reader(path: &Path, c: Compression) -> Result<Box<dyn Read>> {
    let f = File::open(path)?;
    Ok(match c {
        Compression::None => Box::new(BufReader::new(f)),
        Compression::Gz   => Box::new(flate2::read::GzDecoder::new(BufReader::new(f))),
        Compression::Zst  => Box::new(zstd::stream::Decoder::new(BufReader::new(f))?),
    })
}
```

(Inspirado en T16-06 §5.5; T11-04 §D2; T19-04; T08-09.)

---

### D.8. §5 / §19 Redaction (mejoras y patrones)

#### D.8.1. Patrones adicionales al `§5.2`

```text
# Patrones adicionales para T01-06 §5.2
# (los 25 originales se mantienen; los siguientes son incrementales)
ANTHROPIC_API_KEY=sk-ant-[A-Za-z0-9_\-]{20,}
OPENAI_API_KEY=sk-[A-Za-z0-9]{20,}
GEMINI_API_KEY=AIzaSy[A-Za-z0-9_\-]{20,}
HUGGINGFACE_HUB_TOKEN=hf_[A-Za-z0-9]{20,}
REPLICATE_API_TOKEN=r8_[A-Za-z0-9]{20,}
ELEVENLABS_API_KEY=[a-f0-9]{32}
# SSH private keys (multilinea)
-----BEGIN [A-Z ]*PRIVATE KEY-----[\\s\\S]+?-----END [A-Z ]*PRIVATE KEY-----
# PEM certificates
-----BEGIN CERTIFICATE-----[\\s\\S]+?-----END CERTIFICATE-----
# Connection strings
(?i)(postgres|postgresql|mysql|mongodb|redis|amqp)://[^\\s"']{8,}
# IP privadas (PII)
(?<![\\d.])(?:10|127|192\\.168|172\\.(?:1[6-9]|2\\d|3[01]))\\.\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}(?![\\d.])
# Email
[\\w.+-]+@[\\w-]+\\.[\\w.-]+
# Tarjetas de crédito (Luhn-free heuristic)
\\b(?:\\d[ -]?){13,16}\\b
```

(Inspirado en T16-06 §5.5; T20-01 §5.4; T18-06; T13-09 §11.1; T00-10; T00-08.)

**Implementación Moagan (sub-fase L):** implementado en
`src/redact/patterns.rs` con marcadores estables por patrón y tests
unitarios de match/no-match para cada patrón nuevo. ElevenLabs usa
límites de palabra alrededor de los 32 hex para no corromper hashes
SHA-256 de telemetría. La expresión de IP privada usa alternativas con
`\b` porque la crate `regex` de Rust no soporta look-around; cubre las
mismas redes privadas sin consumir caracteres adyacentes.

#### D.8.2. Sustitución por categoría de patrón

```rust
// src/redact/substitute.rs (nuevo)
pub fn substitute(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::SkCpApiKey    => "***REDACTED:api_key:sk-cp***",
        PatternKind::GithubPat     => "***REDACTED:github_pat***",
        PatternKind::AwsAccessKey  => "***REDACTED:aws_access_key***",
        PatternKind::Jwt           => "***REDACTED:jwt***",
        PatternKind::BearerHeader  => "Bearer ***REDACTED***",
        PatternKind::PasswordKv    => "***REDACTED:password***",
        PatternKind::Email         => "***REDACTED:email***",
        PatternKind::PrivateIp     => "***REDACTED:ip***",
        PatternKind::CreditCard    => "***REDACTED:cc***",
        PatternKind::PrivateKey    => "***REDACTED:private_key***",
        PatternKind::ConnString    => "***REDACTED:connstring***",
    }
}
```

(Inspirado en T20-01; T13-09.)

> **Implementación v0.4 — sub-fase K.7 (commit `6cf6bea`).**
> `src/redact/patterns.rs` ships el `PatternKind` enum con 14
> variantes (las 11 del spec + `AnthropicApiKey`, `OpenaiApiKey`,
> `GeminiApiKey` que faltaban en el listado original, + `Unknown`
> como catch-all). La función `substitute(kind)` reproduce
> exactamente los marcadores del spec, con `BearerHeader` devolviendo
> `Bearer ***REDACTED***` para que el header se lea como valor
> completo. `apply_with_categories(policy, surface, text)` en
> `src/redact/apply.rs` recorre los patrones activos y devuelve un
> `RedactResult { text, kinds: Vec<(PatternKind, usize)> }` listo
> para persistir en la tabla `redact_audit` (D.8.5 / D.5.1). 5 unit
> tests cubren cada variante nombrada; los patrones sin mapeo al
> catálogo categorizado se saltan silenciosamente (la `apply()`
> legacy sigue cubriéndolos).

#### D.8.3. Redacción en `ReportingLayer` de `tracing`

```rust
// src/telemetry/redact.rs::ReportingLayer (nuevo)
pub struct ReportingLayer<Inner> { inner: Inner }
impl<S, Inner> Layer<S> for ReportingLayer<Inner>
where Inner: Layer<S>, S: tracing::Subscriber
{
    fn on_event(&self, ev: &Event<'_>, ctx: Context<'_, S>) {
        let mut visitor = RedactingVisitor::default();
        ev.record(&mut visitor);
        self.inner.on_event(&visitor.rebuild_event(ev), ctx);
    }
}
```

(Inspirado en T05-02 §11.4; T19-07 §9.4.)

**Implementación Moagan (sub-fase L):** `ReportingLayer` se integra
como `MakeWriter` de `tracing_subscriber::fmt::Layer` y envuelve el
writer con `RedactWriter`. Así se redactan el mensaje, los campos y el
formato completo antes de escribir bytes al destino. `tracing::Event` es
inmutable, por lo que el límite de salida es la alternativa segura a
reconstruir el evento; los tests cubren claves Anthropic y emails.

#### D.8.4. Redacción post-hoc con `moagan telemetry cleanup --redact-rewrite`

```rust
// src/cli/telemetry_cmd.rs (extender TelemetryCmd::Cleanup)
Cleanup {
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    redact_rewrite: bool,
    #[arg(long)]
    yes: bool,
}
```

(Inspirado en T09-08 §9.3; T18-06 §2.1; T19-07 §9.4.)

#### D.8.5. `redact_audit` table

```sql
-- ver §D.5.1
```

Lleva cuenta de `pattern_kind → match_count` por archivo, para detectar fugas.

#### D.8.6. Redact en `panic` hook

```rust
// src/main.rs (extender)
std::panic::set_hook(Box::new(|info| {
    let msg = format!("{info}");
    let redacted = redact::apply(&msg);
    eprintln!("{redacted}");
}));
```

(Inspirado en T08-10 §13.)

**Implementación Moagan (sub-fase L):** `src/main.rs` instala el hook
inmediatamente después de inicializar `tracing`, extrae payloads `&str`
y `String`, conserva ubicación y aplica la política de redacción de
telemetría antes de escribir a stderr. El test de integración ejecuta el
binario real en debug y verifica que una clave Anthropic no aparece en la
salida.

#### D.8.7. Sanitización de `path` en errores

```rust
// src/error.rs (nuevo variante)
#[error("io: {path:?}: {source}")]
IoPath { path: PathBuf, source: std::io::Error },
```

(Inspirado en T20-09 §5.1.)

---

### D.9. §6 Parallelism (mejoras)

#### D.9.1. `ParallelismGate` con permisos `acquire_many_owned` (P — ✅ API exacta)

```rust
// src/execution/parallelism.rs (extender Parallelism)
impl Parallelism {
    pub async fn acquire_many_owned(&self, n: u32) -> Result<Vec<OwnedSemaphorePermit>>;
    pub fn in_use_atomic(&self) -> usize;
    pub fn saturation_events(&self) -> u64;
}
```

(Inspirado en T00-05 §5.2; T16-09 §4.1; T17-10 §6.2; T11-07 §9.2.)

#### D.9.2. Saturación con back-pressure

```rust
// src/execution/parallelism.rs
impl Parallelism {
    pub async fn acquire_with_backpressure(&self, n: u32) -> Result<Vec<OwnedSemaphorePermit>> {
        if self.recent_saturation() > self.max_saturation_threshold() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        self.acquire_many_owned(n).await
    }
}
```

(Inspirado en T00-08; T16-09 §4.1.)

#### D.9.3. Per-fase parallelism hint

```rust
// src/phases/phase.rs (extender trait)
pub trait Phase: Send + Sync {
    fn name(&self) -> &str;
    fn desired_parallelism(&self) -> usize;
    fn timeout(&self) -> Duration { Duration::ZERO }
    fn failure_policy(&self) -> FailurePolicy { FailurePolicy::Abort }
    fn cost_hint(&self) -> u64 { 0 }
    fn is_required(&self, mode: &Mode) -> bool { true }
    fn cognitive_check(&self) -> CognitiveCheck { CognitiveCheck::Plain }
}

pub enum FailurePolicy { FailFast, SkipNonCritical, Defer, Abort }
pub enum CognitiveCheck { Plain, Contradiction, Ambiguity, HighRisk, Selection, EndOfDiscovery }
```

(Inspirado en T00-10 §3.14; T09-10 §3.1; T18-07 §9.1; T19-09.)

#### D.9.4. `JoinSet` para bounded backpressure

```rust
// src/execution/scheduler.rs (nuevo)
pub async fn run_with_join_set<T, F>(tasks: Vec<F>, max_concurrent: usize) -> Result<Vec<T>>
where F: Future<Output = Result<T>> + Send + 'static, T: Send + 'static
{
    let sem = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut set = tokio::task::JoinSet::new();
    for t in tasks.into_iter() {
        let sem = sem.clone();
        set.spawn(async move {
            let permit = sem.acquire().await?;
            let res = t.await;
            drop(permit);
            res
        });
    }
    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        out.push(res??);
    }
    Ok(out)
}
```

(Inspirado en T07-02 §4.3; T02-07; T01-07; T20-05.)

#### D.9.5. `SaturationEvent` con `requested` y `granted`

```rust
// src/execution/parallelism.rs::record_saturation
pub fn record_saturation(&self, phase: &str, requested: usize, granted: usize) {
    self.saturation_events.fetch_add(1, Ordering::SeqCst);
    telemetry::append_phase(&self.events_path, &PhaseEvent::Saturation {
        phase: phase.into(), requested, granted, at: now(),
    }).ok();
}
```

(Inspirado en T00-08; T16-09 §4.1.)

#### D.9.6. Per-provider semaphores

```rust
// src/llm/registry.rs (nuevo)
pub struct ProviderPool {
    by_name: HashMap<String, Arc<Provider>>,
    global_sem: Arc<tokio::sync::Semaphore>,
    per_provider_sem: HashMap<String, Arc<tokio::sync::Semaphore>>,
    health: HealthTracker,
    round_robin: AtomicUsize,
}
```

(Inspirado en T06-09 §5.4; T20-05 §0.2 D14; T08-03 §5.8.)

#### D.9.7. Token bucket por provider

```rust
// src/llm/rate_limit.rs (nuevo)
pub struct RateLimiter {
    tokens: AtomicU64,
    max_tokens: u64,
    refill_per_sec: u64,
    last_refill: AtomicU64,
}
impl RateLimiter {
    pub fn try_acquire(&self, cost: u64) -> Option<Duration>;
}
```

(Inspirado en T08-01 §6.2; T20-08 §7.8; T15-01 §5.8.)

---

### D.10. §6.4 / §21 Cancellation (mejoras)

#### D.10.1. Cancelación 3-tier

```rust
// src/cancel.rs (extender)
pub enum CancelKind { Cooperative, Hard, Force }
impl CancellationToken {
    pub fn cancel_with(&self, kind: CancelKind) {
        self.cancel();
        if matches!(kind, CancelKind::Hard | CancelKind::Force) {
            self.notify_listeners();
        }
    }
}
```

- **Cooperative**: tasks terminan al final de la iteración actual.
- **Hard**: tasks en I/O se interrumpen (socket close).
- **Force**: run se marca `failed`, no se permite `continue`.

(Inspirado en T01-03 §2.3; T19-07 §9.4.)

**Implementación Moagan (sub-fase L):** `src/cancel.rs` añade
`CancelTier::{Soft, Normal, Hard}` y `Cancel::cancel_with_tier`. En esta
fase los tres tiers convergen deliberadamente en el token cooperativo
existente: no hay todavía un `CancellationContext` ni un registro de
procesos hijo desde el que enviar SIGTERM equivalente. La limitación
queda cubierta por el contrato y el test de los tres tiers.

#### D.10.2. Listeners de cancelación

```rust
// src/cancel.rs
impl CancellationToken {
    pub fn on_cancel(&self, cb: impl FnOnce() + Send + 'static) -> ListenerGuard;
}
```

(Inspirado en T16-06 §2.6; T00-05 §4.6.)

#### D.10.3. `CancellationTree` parent/child

```rust
// src/cancel.rs
pub struct CancellationTree { root: CancellationToken }
impl CancellationTree {
    pub fn child(&self) -> CancellationToken;
    pub fn cancel_subtree(&self, phase: &str);
}
```

Cuando run se cancela, todos los child tokens se cancelan. (Inspirado en T06-04 §6.4; T07-04 §8.4; T11-07 §9.2.)

#### D.10.4. `Drop` cleanup para `CancellationContext`

```rust
// src/cancel.rs
pub struct CancellationContext<'a> { token: CancellationToken, _lifetime: PhantomData<&'a ()> }
impl Drop for CancellationContext<'_> {
    fn drop(&mut self) {
        if !self.token.is_cancelled() {
            telemetry::append_phase(&self.events_path, &PhaseEvent::Cancelled { at: now() }).ok();
        }
    }
}
```

(Inspirado en T17-09 §1205-1230.)

#### D.10.5. `tokio::select!` con cancelación

```rust
// patrón recomendado (T20-06 §6.5)
let response = tokio::select! {
    r = client.post(&url).send() => r?,
    _ = cancel.cancelled() => return Err(Error::Cancelled),
}?;
```

(Inspirado en T20-06 §6.5.)

#### D.10.6. Kill child processes on cancel

```rust
// src/sandbox/process.rs (extender)
pub async fn run(cmd: Command, cancel: CancellationToken) -> Result<SandboxResult> {
    let mut child = cmd.spawn()?;
    let pid = child.id();
    cancel.on_cancel(move || {
        unsafe { libc::killpg(pid as i32, libc::SIGTERM); }
    });
}
```

(Inspirado en T07-07 §10.1; T03-05 §3.8; T08-01 §6.2.)

#### D.10.7. Cancelación con `child_token()`

```rust
// src/phases/phase.rs
async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
    let child = ctx.cancel.child_token();
    let phase_token = ctx.cancel.clone();
    tokio::select! {
        r = self.real_execute(ctx.with_token(child)) => r,
        _ = phase_token.cancelled() => Err(Error::PhaseCancelled(self.name().into())),
    }
}
```

(Inspirado en T06-04 §6.4.)

#### D.10.8. Cancel mid-retry

```rust
// src/llm/retry.rs::backoff_aware
pub async fn sleep_or_cancel(d: Duration, cancel: &CancellationToken) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(d) => Ok(()),
        _ = cancel.cancelled() => Err(Error::Cancelled),
    }
}
```

(Inspirado en T02-07 §5.6.)

---

### D.11. §7 Sandbox (endurecimiento)

#### D.11.1. cgroup v2 + `prlimit` fallback

```rust
// src/sandbox/process.rs (Linux)
#[cfg(target_os = "linux")]
pub fn apply_resource_limits(cmd: &mut Command, cpu_s: u64, mem_mb: u64) -> Result<()> {
    use nix::sys::resource::{setrlimit, Resource};
    unsafe {
        cmd.pre_exec(move || {
            setrlimit(Resource::RLIMIT_CPU, cpu_s, cpu_s)?;
            setrlimit(Resource::RLIMIT_AS, mem_mb * 1024 * 1024, mem_mb * 1024 * 1024)?;
            setrlimit(Resource::RLIMIT_NOFILE, 256, 256)?;
            setrlimit(Resource::RLIMIT_FSIZE, 100 * 1024 * 1024, 100 * 1024 * 1024)?;
            Ok(())
        });
    }
    Ok(())
}
```

(Inspirado en T13-04 §6.6; T18-06 §3.3; T09-09 §1.2; T02-04.)

#### D.11.2. `unshare(CLONE_NEWNS|NEWPID|NEWNET|NEWIPC|NEWUTS)`

```rust
// src/sandbox/process.rs (Linux, opt-in)
#[cfg(all(target_os = "linux", feature = "namespace"))]
pub fn apply_namespace(cmd: &mut Command) -> Result<()> {
    use nix::sched::{unshare, CloneFlags};
    cmd.pre_exec(|| { unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID |
                              CloneFlags::CLONE_NEWNET | CloneFlags::CLONE_NEWIPC |
                              CloneFlags::CLONE_NEWUTS).map(|_| ()) });
    Ok(())
}
```

Opt-in vía `MOAGAN_SANDBOX_NET=allow` o `--allow-local-validation`. (Inspirado en T18-06 §3.3; T07-05 §1.7.)

#### D.11.3. Denylist explícito

```rust
// src/sandbox/allowlist.rs
pub const DENYLIST: &[&str] = &[
    "curl", "wget", "ssh", "scp", "rsync", "nc", "ncat", "socat",
    "bash", "sh", "-c", "eval", "exec", "source",
];
```

Verifica con `shlex::split(cmd)` que no aparezca ningún denylist como argv. (Inspirado en T07-05 §1.7.)

#### D.11.4. Cap stdout/stderr

```rust
// src/sandbox/process.rs
pub const DEFAULT_OUTPUT_CAP_BYTES: usize = 64 * 1024;
pub const MAX_STDOUT_BYTES: usize = DEFAULT_OUTPUT_CAP_BYTES;
pub const MAX_STDERR_BYTES: usize = DEFAULT_OUTPUT_CAP_BYTES;
```

Truncar en `SandboxResult.stdout_summary`. (Inspirado en T08-10 §5.5; T00-06 §11.3.)

**N status (implemented):** `DEFAULT_OUTPUT_CAP_BYTES` is 64 KiB.
The sandbox drains stdout and stderr independently, kills the child when
either stream exceeds its cap, and returns
`SandboxError::OutputTruncated` instead of silently accepting a partial
result. `SandboxConfig::with_max_capture` remains the per-run override.

#### D.11.5. Per-command config

```toml
# ~/.config/moagan/sandbox.toml
[sandbox.rust]
allowed_commands = ["cargo", "rustc", "rustup"]
network = "deny"
cpu_seconds = 60
memory_mb = 1024

[sandbox.python]
allowed_commands = ["python", "python3", "pip"]
network = "deny"
cpu_seconds = 30
memory_mb = 512

[sandbox.typescript]
allowed_commands = ["tsc", "node"]
network = "deny"
cpu_seconds = 30

[sandbox.sql]
allowed_commands = ["sqlite3", "psql"]
network = "deny"
cpu_seconds = 15

[sandbox.schema]
allowed_commands = ["jq", "yq"]
network = "deny"
```

(Inspirado en T05-01; T20-01.)

**N status (implemented):** `COMMAND_CONFIGS` provides the four static
profiles (`rust`, `python`, `typescript`, `sql`) and `config_for(name)`
performs the logical-name lookup. Each profile records argument limits,
per-stream output cap, timeout, and network metadata without adding a
configuration dependency.

#### D.11.6. `strip_secrets()` antes de spawn

```rust
// src/sandbox/process.rs
pub fn strip_secrets(env: &mut HashMap<String, String>) {
    for k in env.keys().cloned().collect::<Vec<_>>() {
        let kl = k.to_lowercase();
        if kl.contains("key") || kl.contains("token") || kl.contains("secret") || kl.contains("password") {
            env.remove(&k);
        }
    }
    env.insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into());
    env.insert("HOME".into(), work_dir.to_string_lossy().into());
}
```

(Inspirado en T03-01; T00-06 §11.2.)

**N status (implemented):** `strip_secrets(&[String])` runs the shared
redaction policy over secret-looking argv values immediately before
`Command::spawn`. It preserves the argument layout and covers MiniMax,
Anthropic, generic OpenAI-style, Gemini, Hugging Face, Replicate,
GitHub, Slack, and Bearer prefixes. The existing environment scrubber
continues to run independently.

#### D.11.7. `seccomp` whitelist (Linux, opt-in)

```rust
// src/sandbox/seccomp.rs (nuevo, feature `seccomp`)
pub fn apply_seccomp_whitelist() -> Result<()> {
    // deny socket(AF_INET) connect/accept
    // deny ptrace
    // allow read, write, exit, exit_group
}
```

(Inspirado en T18-06 §3.3; T07-05 §1.7.)

#### D.11.8. `NotAllowed` como estado distinto

```rust
// src/validators/structural.rs
pub enum ValidationStatus { Pass, Warn, Fail, Skipped, NotAllowed, Error }
```

(Inspirado en T07-07 §10.3; T20-01; T19-09.)

#### D.11.9. Off-by-default network para validación Rust

```rust
// src/validators/rust_validator.rs
pub async fn check(artifact: &Artifact, sandbox: &Sandbox) -> Result<ValidationEvidence> {
    let sb = sandbox.fork_with_env("CARGO_NET_OFFLINE", "true");
}
```

(Inspirado en T05-09 §5.8; T07-05 §1.7.)

#### D.11.10. `--allow-injection` flag (escape de validación)

```rust
// src/cli/run.rs
#[arg(long)]
allow_injection: bool,
```

(Inspirado en T04-07 §7.2.)

#### D.11.11. Watchdog que mata árbol de procesos

```rust
// src/sandbox/process.rs
pub async fn run_with_watchdog(cmd: &mut Command, timeout: Duration) -> Result<SandboxResult> {
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    let killer = tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        unsafe { libc::killpg(pid as i32, libc::SIGKILL); }
    });
    let out = child.wait_with_output().await?;
    killer.abort();
    Ok(SandboxResult::from(out))
}
```

(Inspirado en T07-07 §10.1.)

#### D.11.12. Capturar tool versions para reproducibilidad

```rust
// src/sandbox/process.rs
pub async fn capture_tool_versions(sandbox: &Sandbox) -> Result<HashMap<String, String>> {
    let mut versions = HashMap::new();
    for tool in ["cargo", "rustc", "python3", "node", "tsc", "sqlite3"] {
        if let Ok(r) = sandbox.run(tool, &["--version"]).await {
            versions.insert(tool.into(), r.stdout.trim().into());
        }
    }
    Ok(versions)
}
```

Persistir en `manifest.json` o `validation/<p_id>.json`. (Inspirado en T00-06 §11.4.)

#### D.11.13. `MoaSandbox` con `NetworkPolicy`

```rust
// src/sandbox/mod.rs
pub struct Sandbox {
    work_dir: TempDir,
    network: NetworkPolicy,
    cpu_seconds: u64,
    memory_mb: u64,
    max_files: u64,
    allowlist: Vec<String>,
    denylist: Vec<String>,
}
pub enum NetworkPolicy { NoNetwork, AllowList(Vec<String>) }
```

(Inspirado en T07-07 §10.1; T18-06 §3.3.)

#### D.11.14. `Validator` con campos tipados

```rust
// src/validators/mod.rs
pub struct Validator {
    pub kind: ValidatorKind,
    pub cpu_limit: Duration,
    pub mem_limit_mb: u64,
    pub allowed_commands: Vec<String>,
    pub network: NetworkPolicy,
    pub env_overrides: HashMap<String, String>,
}
```

(Inspirado en T03-01; T07-07 §10.1.)

#### D.11.15. `Sandbox::run` con `Command` struct

```rust
// src/sandbox/mod.rs (extender)
pub async fn run(&self, cmd: &Command) -> Result<RunReport>;

pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    pub env: HashMap<String, String>,
}
pub struct RunReport {
    pub exit_code: i32,
    pub stdout_summary: String,
    pub stderr_summary: String,
    pub duration: Duration,
    pub tool_versions: HashMap<String, String>,
}
```

(Inspirado en T11-06 §5.6; T07-10 §2111; T20-01.)

#### D.11.16. Verificar que el binario existe antes de spawn

```rust
// src/sandbox/process.rs
pub fn ensure_in_allowlist(program: &str) -> Result<()> {
    let resolved = which::which(program)
        .map_err(|_| Error::NotAllowed(program.into()))?;
    if !AllowList::global().contains(&resolved.to_string_lossy()) {
        return Err(Error::NotAllowed(resolved.to_string_lossy().into()));
    }
    Ok(())
}
```

(Inspirado en T20-01; T07-07 §10.1.)

**N status (implemented):** `verify_binary_exists` resolves absolute paths
and PATH entries before spawn and returns the typed
`SandboxError::BinaryNotFound(String)` when resolution fails. The
legacy `SandboxStatus::NotFound` result is retained for validator
compatibility and for a binary disappearing between preflight and spawn.

---

### D.12. §8 Non-discovery pipeline (refinamientos)

#### D.12.1. `PhaseResult<T>` enriquecido

```rust
// src/phases/phase.rs
pub struct PhaseResult<T> {
    pub output: T,
    pub budget_consumed: u64,
    pub artifacts_written: Vec<PathBuf>,
    pub warnings: Vec<Warning>,
    pub telemetry: HashMap<String, Value>,
    pub next: PhaseDirective,
}
pub enum PhaseDirective { Continue, Pause, Abort, BranchTo(String) }
```

(Inspirado en T03-01 §9.1; T18-10 §7.1; T07-10 §1104.)

#### D.12.2. `selection_sketches` antes de proposal

```rust
// src/phases/proposal.rs (extender)
pub async fn proposal_phase(brief: &CanonicalBrief, sketches: &[Sketch], ...) -> Result<Vec<Proposal>> {
    let selected = select_sketches(sketches, &SelectionPlan::for_mode(brief.mode))?;
}
```

(Inspirado en T14-09 §847-867; T15-02 §7; T01-10 §5.5.)

#### D.12.3. `epistemic_legacy` injection

```rust
// src/phases/proposal.rs
pub async fn proposal_phase(...) -> Result<Vec<Proposal>> {
    let legacy = read_epistemic_legacy(brief.run_id)?;
    let prompt = render("proposer", (&brief, &sketch, &legacy))?;
}
```

(Inspirado en T14-09; T19-04 §6.5.)

#### D.12.4. `select_sketches` con `SelectionPlan`

```rust
// src/ranking/diversity.rs
pub struct SelectionPlan {
    pub keep_top: usize,
    pub keep_diverse: usize,
    pub keep_outlier: usize,
    pub min_quality_score: f32,
}
impl SelectionPlan {
    pub fn for_mode(mode: &Mode) -> Self {
        match mode {
            Mode::Fast      => Self { keep_top: 2, keep_diverse: 0, keep_outlier: 0, min_quality_score: 0.5 },
            Mode::Standard  => Self { keep_top: 2, keep_diverse: 1, keep_outlier: 0, min_quality_score: 0.6 },
            Mode::Deep      => Self { keep_top: 3, keep_diverse: 2, keep_outlier: 1, min_quality_score: 0.7 },
            Mode::Explore   => Self { keep_top: 5, keep_diverse: 5, keep_outlier: 1, min_quality_score: 0.4 },
            Mode::Batch     => Self { keep_top: 3, keep_diverse: 2, keep_outlier: 0, min_quality_score: 0.6 },
            Mode::Discovery => Self { keep_top: 0, keep_diverse: 0, keep_outlier: 0, min_quality_score: 0.0 },
        }
    }
}
```

(Inspirado en T14-09; T15-02; T19-04; T01-10 §5.5.)

#### D.12.5. Adversary separado del Judge

```rust
// src/phases/adversary.rs
pub async fn adversary_phase(p: &Proposal, ranking: &Ranking) -> Result<AdversarialReport> {
    // 5 patrones:
    //   shared_blind_spots, unanimous_claims_without_evidence, hidden_assumptions,
    //   omitted_risks, unverified_claims
}
```

(Inspirado en T14-09 §1107-1128; T00-03 §1107-1128; T06-08 §10.5.)

#### D.12.6. `keep_better` después de repair

```rust
// src/phases/repair.rs
pub fn keep_better(original: &Proposal, revised: &Proposal) -> Proposal {
    let orig_score = original.blockers.len() * 2 + original.majors.len();
    let rev_score  = revised.blockers.len() * 2 + revised.majors.len();
    if rev_score < orig_score { revised.clone() }
    else if rev_score == orig_score && revised.thesis_similarity(&original.thesis) < 0.95 { revised.clone() }
    else { original.clone() }
}
```

(Inspirado en T05-06 §F9 D27-28.)

#### D.12.7. Doble-ciego en critique/judge

```rust
// src/phases/critique.rs
pub async fn critique_phase(p: &Proposal, db: &Db) -> Result<Vec<Critique>> {
    let roles = critic_assignment(p);
    let tasks = roles.iter().map(|role| {
        let p = anonymize(p);
        async move { /* send critique request */ }
    });
    let all = futures::future::join_all(tasks).await;
}
```

(Inspirado en T00-06 §12.1; T05-06 §F8; T05-09; T07-05 §1.9.)

#### D.12.8. Stable `ErrorCode` enum (30+ variantes)

```rust
// src/error.rs (extender)
pub enum ErrorCode {
    FsNotFound, ProviderAuth, ProviderRateLimit, CheckpointRejected,
    DiscoverySaturated, InternalInvariant,
    Http400, Http401, Http403, Http404, Http408, Http413,
    Http429, Http500, Http502, Http503, Http504,
    TransportError, JsonInvalid, SchemaViolation, Truncated,
    TimeoutSketch, TimeoutPhase, TimeoutTotal,
    BudgetExhaustedIntake, BudgetExhaustedDecomposition, BudgetExhaustedSketches,
    BudgetExhaustedFullProposals, BudgetExhaustedCriticism, BudgetExhaustedRepair,
    BudgetExhaustedValidation, BudgetExhaustedJudging, BudgetExhaustedSynthesis,
    Cancelled, PlanPaused, PlanHardLimit, CircuitOpen,
    ConflictMonolithMicroservices, ConflictSyncEventDriven, ConflictSqlNoSql,
    ConflictStrongEventual, ConflictSelfHostedServerless, ConflictRustNonPermittedRuntime,
    SandboxNotAllowed, SandboxTimeout, SandboxNoBinary, SandboxOom, SandboxKilled,
    ProviderOverloaded, QuotaExceeded, ContentFiltered, InvalidResponse, StreamingNotSupported,
    DiscoveryInsufficient, RepairUnrecoverable, GateFailed, ValidationUnrunnable,
    ContextRefNotFound, ContextRefInvalid, InputTooLarge, PromptInjectionSuspected,
    HostilePrompt, ManifestInconsistent, ExportVerificationFailed, OutOfDiskSpace,
    LlmUnsupportedEndpoint, UnhandledError, NeedsInput,
}
impl ErrorCode {
    pub fn stable(&self) -> &'static str;
    pub fn is_retriable(&self) -> bool;
    pub fn is_circuit_opening(&self) -> bool;
}
```

(Inspirado en T15-01 §0.4; T14-09; T18-06 §3.5; T19-07; T20-04.)

#### D.12.9. `MoaError` con `retriable: bool` y `source`

```rust
// src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum MoaError {
    #[error("config: {0}")]
    Config(String),
    #[error("storage: {0}")]
    Storage(StorageError),
    #[error("llm: {0}")]
    Llm(LlmError),
    #[error("validation: {0}")]
    Validation(ValidationError),
    #[error("budget exhausted: phase={phase}, allocated={allocated}, spent={spent}")]
    BudgetExhausted { phase: String, allocated: u64, spent: u64 },
    #[error("cancelled")]
    Cancelled,
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("io: path={path:?}, source={source}")]
    IoPath { path: PathBuf, #[source] source: std::io::Error },
    #[error("manifest inconsistent: {0}")]
    ManifestInconsistent(String),
    #[error("export verification failed: {0}")]
    ExportVerificationFailed(String),
    #[error("needs input: {0}")]
    NeedsInput(String),
    #[error("prompt injection suspected: {0}")]
    PromptInjectionSuspected(String),
    #[error("hostile prompt: {0}")]
    HostilePrompt(String),
}
```

(Inspirado en T00-08 §1363; T19-10 §14.2; T13-10 §14.1; T20-04; T20-09 §5.1; T20-10 §4.1.)

#### D.12.10. `StorageError` con sub-variantes

```rust
#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("sqlite: {0}")] Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")] Json(#[from] serde_json::Error),
    #[error("toml: {0}")] Toml(#[from] toml::de::Error),
    #[error("hash mismatch: file={file}, expected={expected}, got={actual}")]
    HashMismatch { file: String, expected: String, actual: String },
    #[error("atomic write failed: path={path}, attempts={attempts}")]
    AtomicWriteFailed { path: PathBuf, attempts: u8 },
    #[error("out of disk space: needed={needed}, free={free}")]
    OutOfDiskSpace { needed: u64, free: u64 },
}
```

(Inspirado en T13-10; T19-10; T00-08.)

#### D.12.11. `LlmError` con `RetryAdvice`

```rust
#[derive(thiserror::Error, Debug)]
pub enum LlmError {
    #[error("transport: {0}")] Transport(#[from] reqwest::Error),
    #[error("auth: provider={provider}")] Auth { provider: String },
    #[error("rate limited: retry_after={retry_after:?}")] RateLimited { retry_after: Option<Duration> },
    #[error("quota exceeded")] QuotaExceeded,
    #[error("circuit open")] CircuitOpen,
    #[error("server error: status={status}")] ServerError { status: u16 },
    #[error("client error: status={status}, message={message}")] ClientError { status: u16, message: String },
    #[error("invalid response: {0}")] InvalidResponse(String),
    #[error("timeout: {0:?}")] LlmTimeout(Duration),
    #[error("truncated: partial={partial:?}")] Truncated { partial: String },
    #[error("schema validation: {0}")] SchemaValidation(String),
    #[error("content filtered: {0}")] ContentFiltered(String),
    #[error("provider overloaded")] ProviderOverloaded,
    #[error("budget exhausted: provider={provider}, would_use={would_use}")]
    BudgetExhausted { provider: String, would_use: u64 },
    #[error("streaming not supported")] StreamingNotSupported,
    #[error("cancelled")] Cancelled,
    #[error("contract violation: {0}")] ContractViolation(String),
}
impl LlmError {
    pub fn retry_advice(&self) -> RetryDecision;
}
pub enum RetryDecision { RetryWithBackoff, WaitAndRetry, DoNotRetry, Truncate }
```

(Inspirado en T20-06 §6.4; T19-07 §4.1; T18-06 §3.5; T08-03 §5.3; T17-09; T01-10 §3.1.)

#### D.12.12. Stable `ErrorCode` con serialización

```rust
// src/error.rs
impl serde::Serialize for ErrorCode { /* SCREAMING_SNAKE */ }
```

(Inspirado en T15-01 §0.4.)

#### D.12.13. `api_keys.toml` precedence

```
1. env:VAR (highest)
2. file:/path
3. interactive (if --interactive)
4. literal (only if privacy.allow_literal=true, default false)
```

(Inspirado en T20-02 §11.2; T11-02 §5.5.)

#### D.12.14. Códigos de salida del CLI

```text
0   ok
1   error genérico
2   argumentos inválidos
3   api key inválida
4   plan exhausted (también "needs_input" en batch)
5   timeout
6   cancelled
7   schema violation persistente
8   io error
9   budget exhausted
10  needs_input
20  budget_exceeded
30  plan_paused
40  provider_error
50  timeout
60  invalid_args
70  io_error
80  context_error
90  export_verification_failed
130 SIGINT
```

(Inspirado en T15-01 §14.4; T18-06; T20-03 §14.3; T13-03 §12.4; T14-07 §12.4.)

**Implementación Moagan (sub-fase L):** `src/error.rs` define
`ExitCode` con discriminantes `repr(i32)` y `Error::exit_code()`. Los
errores base conservan los códigos 0–8; provider/state/cache usan los
códigos extendidos del catálogo. `cli::dispatch` convierte los errores a
un `i32` de salida y mantiene un único mensaje de error en stderr.

#### D.12.15. `PauseReason` tipado

```rust
// src/domain/run.rs
pub enum PauseReason {
    HumanCheckpoint,
    TimeoutPhase,
    TimeoutTotal,
    PlanExceeded,
    BudgetExhausted,
    ProviderError,
    UserPause,
    HostilePrompt,
    NeedsInput,
}
```

(Inspirado en T18-09; T02-03 §2.2; T18-08 §2.1; T20-09 §5.1.)

**Implementación Moagan (sub-fase L):** como el crate usa el módulo
plano `src/domain.rs` en vez de `src/domain/run.rs`, `PauseReason` vive
allí y serializa con `snake_case`. `CancelReason` implementa `From` hacia
el estado de pausa más cercano, incluyendo timeout de fase/total, plan
exhausted, provider error y user pause.

#### D.12.16. `RunPaths::resolve()` con relative+absolute

```rust
// src/storage/run_fs.rs
pub struct RunPaths {
    pub rel: HashMap<&'static str, &'static str>,
    pub abs: HashMap<&'static str, PathBuf>,
}
impl RunPaths {
    pub fn resolve(&self, key: &str, base: &Path) -> PathBuf;
    pub fn apparent_norm_path(&self, key: &str) -> Option<PathBuf>;
}
```

(Inspirado en T10-04 §7.3; T11-08 §7.4; T07-10 §748.)

---

### D.13. §9 Discovery (refinamientos)

#### D.13.1. `StopDecision` explícito

```rust
// src/discovery/saturation.rs
pub enum StopDecision { Continue, QueueExtra(usize), SwitchModel(String) }
pub struct StopPolicy {
    pub saturation_threshold: f32,
    pub cola_reserva: f32,
    pub outlier_distance: u32,
    pub min_sketches: usize,
    pub max_sketches: usize,
    pub hard_cap: usize,
    pub allow_oversized: bool,
}
```

(Inspirado en T01-05; T18-04 §4.3; T06-06 D1; T19-09 D15; T16-09 §6.3; T20-09.)

#### D.13.2. `SaturationTracker::coverage()` y `mean_intra_cluster_similarity`

```rust
// src/discovery/saturation.rs
pub struct SaturationTracker {
    pub per_model_saturated: HashMap<ModelId, bool>,
    pub last_aporate: f32,
    pub sketch_count: usize,
    pub cluster_count: usize,
    pub mean_intra_cluster_similarity: f32,
}
impl SaturationTracker {
    pub fn update(&mut self, batch: &[Sketch], clusters: &[Cluster]) -> StopDecision;
    pub fn coverage(&self) -> f32;
    pub fn model_saturated(&self, model: &ModelId) -> bool;
}
```

(Inspirado en T01-09 §6.4; T18-04 §4.3; T19-04 §13.5.)

#### D.13.3. Constantes concretas de discovery

```rust
// src/discovery/config.rs
pub const DEFAULT_SATURATION_THRESHOLD: f32 = 0.05;
pub const DEFAULT_COLA_RESERVA: f32 = 0.25;
pub const DEFAULT_UNCATEGORIZED_THRESHOLD: f32 = 0.30;
pub const DEFAULT_MIN_SKETCHES: usize = 40;
pub const DEFAULT_MAX_CATEGORIES: usize = 12;
pub const DEFAULT_MAX_CATEGORIES_SOFT: usize = 30;
pub const DEFAULT_OUTLIER_DISTANCE_BITS: u32 = 32;
pub const DEFAULT_DISCOVERY_HARD_CAP: usize = 500;
```

(Inspirado en T01-05; T01-06 §0.5#12; T18-04; T19-09; T15-02.)

#### D.13.4. Discovery pre-flight con `tiktoken-rs`

```rust
// src/discovery/matrix.rs
pub fn validate_cardinality(matrix: &ExplorationMatrix, avg_tokens: u64, max_storage_gb: u64) -> Result<()> {
    let total = matrix.cardinality() as u64 * avg_tokens;
    if total > max_storage_gb * 1_000_000_000 {
        return Err(Error::CardinalityExceedsStorage { needed: total, available: max_storage_gb * 1_000_000_000 });
    }
    Ok(())
}
```

(Inspirado en T19-09 D15; T18-04 §4.3; T04-05.)

#### D.13.5. `DiscoveryContext` con `category_id`

```rust
// src/discovery/extractor.rs
pub struct DiscoveryContext {
    pub category_id: CategoryId,
    pub sketch_ids: Vec<SketchId>,
    pub contradiction_ids: Vec<ContradictionId>,
    pub facet_ids: Vec<FacetId>,
    pub brief_hash: String,
    pub matrix_hash: String,
    pub tagger_threshold: f32,
}
```

(Inspirado en T18-09 §8.2; T18-04.)

#### D.13.6. `DiscoveryCoordinator` separado del `Coordinator`

```rust
// src/discovery/mod.rs
pub struct DiscoveryCoordinator { state: DiscoveryState, db: Db }
pub struct StandardCoordinator { state: StandardState, db: Db }
```

(Inspirado en T01-09 §3.1.)

#### D.13.7. `DiscoverySaturated` event

```rust
// src/discovery/saturation.rs
pub fn emit_saturation_event(tracker: &SaturationTracker) {
    if tracker.sketch_count >= tracker.target && tracker.cola_reserva_remaining() == 0 {
        telemetry::append_phase(&tracker.events_path, &PhaseEvent::DiscoverySaturated { at: now() }).ok();
    }
}
```

(Inspirado en T19-07 §9.7; T19-04 §11.9.)

#### D.13.8. `DiscoveryLoop::should_terminate` con `BlockReason`

```rust
// src/discovery/loop.rs
pub enum BlockReason {
    InsufficientResults { threshold: f32, actual: f32 },
    AllModelsSaturated,
    BudgetExhausted,
    Cancelled,
}
impl DiscoveryLoop {
    pub fn should_terminate(&self, state: &DiscoveryState) -> Option<BlockReason>;
}
```

(Inspirado en T03-06 §10.1.)

#### D.13.9. Tagger con threshold configurable

```rust
// src/discovery/tagger.rs
pub struct Tagger { threshold: f32, model: ModelSpec }
impl Tagger {
    pub fn default_threshold() -> f32 { 0.6 }
    pub fn for_low_uncategorized() -> f32 { 0.55 }
}
```

(Inspirado en T18-09; T15-02.)

#### D.13.10. `tag_decision` enum

```rust
// src/discovery/tagger.rs
pub enum TagDecision {
    Categorize(CategoryId),
    CategorizeAsOutlier,
    AssignNewCategory(String),
    SkipBecauseUncertain,
}
```

(Inspirado en T19-04 §17.1.)

#### D.13.11. `ContradictionDetector` con `topic, description, severity`

```rust
// src/discovery/contradiction.rs
pub struct Contradiction { topic: String, description: String, severity: Severity }
pub enum Severity { Low, Medium, High }
```

(Inspirado en T01-06 §9.6 + T18-04.)

#### D.13.12. `Facet` con `required: bool`

```rust
// src/discovery/facet.rs (T01-06 §9.7 ya lo tiene; ampliado)
pub struct Facet { id: String, description: String, required: bool, kind: FacetKind }
pub enum FacetKind { Section, Question, Comparison, Tradeoff }
```

(Inspirado en T18-04; T19-04.)

#### D.13.13. Facet cache global

```rust
// src/discovery/facet_cache.rs (nuevo)
pub struct FacetCache { db: Db, ttl: Duration }
impl FacetCache {
    pub fn get_or_compute(&self, brief: &CanonicalBrief, cat: &Category) -> Result<Vec<Facet>>;
    fn key(&self, brief: &CanonicalBrief, cat: &Category) -> String {
        blake3::hash(&[brief.canonical_json().as_bytes(), cat.id.to_string().as_bytes()].concat())
    }
}
```

Persistido en `~/.local/share/moagan/.runs/_facet_cache/`. (Inspirado en T01-05; T02-04 §7.1; T06-06 D2.)

#### D.13.14. Integrator con 3 pasos

```rust
// src/discovery/integrator.rs
pub async fn integrate(extractions: HashMap<FacetId, Markdown>, brief: &CanonicalBrief) -> Result<Document> {
    let draft = script::join(extractions)?;
    let issues = llm_integrator_validate(&draft).await?;
    if !issues.is_empty() {
        let draft = fuse_contradictions(draft, issues)?;
    }
    let final_doc = llm_refiner(&draft).await?;
    if final_doc.coverage_ratio(&draft) < 0.85 { return Err(Error::RefinerDilutedContent); }
    Ok(final_doc)
}
```

(Inspirado en T01-06 §9.9 + T15-02 §6.11; T11-06.)

#### D.13.15. `HARD_INCOMPATIBILITIES` constant

[x] Shipped: src/domain/constraint.rs (PR #49, fix(b)). 10 pares en la const; consumido por SynthesizePhase::cluster_conflict. Anotación propuesta en docs/q1-v0.3-status-sync.

```rust
// src/domain/constraint.rs
pub const HARD_INCOMPATIBILITIES: &[(&str, &str)] = &[
    ("monolith", "microservices"),
    ("sync_rpc", "event_driven"),
    ("strong_consistency", "eventual_consistency"),
    ("sql", "nosql"),
    ("self_hosted", "serverless"),
    ("rust", "non_permitted_runtime"),
    ("single_tenant", "multi_tenant"),
    ("monolith_db", "polyglot_persistence"),
    ("pull_based", "push_based"),
    ("custom_protocol", "standard_protocol"),
];
pub fn is_incompatible(a: &str, b: &str) -> bool {
    HARD_INCOMPATIBILITIES.iter().any(|(x, y)| (a == *x && b == *y) || (a == *y && b == *x))
}
```

(Inspirado en T02-09; T19-09; T03-01; T18-04 §11.1; T05-10 §11.1; T08-06 §11.2; T08-08 §11.2.)

#### D.13.16. `SynthesisCompeteRule`: replace si domina ≥2 dimensiones

```rust
// src/discovery/synthesizer.rs
pub fn should_replace_synthesis(synthesis: &Proposal, sources: &[Proposal], front: &ParetoFront) -> bool {
    let s_v = synthesis.quality_vector();
    let source_v: Vec<_> = sources.iter().map(|p| p.quality_vector()).collect();
    let dominates_count = source_v.iter().filter(|v| dominates(v, &s_v)).count();
    let s_dominates = source_v.iter().filter(|v| dominates(&s_v, v)).count();
    s_dominates >= 2 && dominates_count == 0
}
```

(Inspirado en T13-04 §10; T18-04; T19-04 §14.8.)

#### D.13.17. `refiner` con invariantes de cobertura

```rust
// src/discovery/integrator.rs::assert_preserves_citations
pub fn assert_preserves_citations(original: &Document, refined: &Document) -> Result<()> {
    let orig_cits: HashSet<_> = original.citations().collect();
    let ref_cits: HashSet<_> = refined.citations().collect();
    let preserved = orig_cits.intersection(&ref_cits).count() as f32 / orig_cits.len() as f32;
    if preserved < 0.9 { return Err(Error::RefinerLostCitations { preserved }); }
    Ok(())
}
```

(Inspirado en T05-01 §8.6; T07-03 §15.1.)

#### D.13.18. `Persona` y `Angle` para diversidad forzada

```rust
// src/discovery/persona.rs
pub struct Persona { id: PersonaId, weight: f32, secondary_constraints: Vec<String> }
pub struct Angle { id: AngleId, weight: f32, persona_ref: PersonaId }
pub struct DiversityVector {
    pub persona: PersonaId,
    pub angle: AngleId,
    pub temperature: f32,
    pub secondary_constraints: Vec<String>,
    pub priority: Priority,
    pub must_not_imitate: Vec<SketchId>,
}
```

`DiversityBook` trackea tuplas usadas para evitar repetición. (Inspirado en T03-04 §2; T06-10 §6.2; T12-09.)

#### D.13.19. `MatrixCell` con `seed` documentada como always-None (P — ✅ SPEC-DRIFT resuelto en v0.5 PR-16)

```rust
// src/discovery/matrix.rs
pub struct MatrixCell {
    pub model: ModelSpec,
    pub role: RoleId,
    pub temperature: f32,
    pub seed: Option<u64>,
}
```

(Inspirado en T16-06 §2.1; T01-10 §6.1.)

> **Estado (v0.5 PR-16, ✅ SPEC-DRIFT resuelto 2026-08):** V4 §6.4 y T01-06 §9.1 especifican la forma `roles × models × temperatures`. La implementación en `src/discovery/matrix.rs:36` usa deliberadamente `dimensions × facets × per_cell`: `MatrixCell` contiene `{ dimension_id, facet_id, label }`, mientras que el `seed` efectivo se deriva externamente, de modo que la tupla por celda es `{ dimension_id, facet_id, label, seed }` y no hay `model_spec`, `role` ni `temperature` por celda. Esta evolución es más flexible porque no exige una instancia de provider diferente para cada celda. La decisión de v0.5 PR-16 (2026-08) es documentar la divergencia, no re-diseñar la matriz; un caso futuro que requiera model/role/temperature por celda será una feature v0.6+, y los futuros contribuidores no deben proponer re-diseñar la matriz sin consultar primero la hoja de ruta v0.5 PR #16.

#### D.13.20. `Cardinality` con `range_usize` per-phase

```rust
// src/discovery/matrix.rs
pub struct Cardinality {
    pub sketches: Range<usize>,
    pub proposals: Range<usize>,
    pub critics_per_proposal: usize,
    pub judges: usize,
    pub repair_rounds: usize,
}
impl Cardinality {
    pub fn for_mode(mode: &Mode) -> Self { /* switch on mode */ }
    pub fn fixed(&self) -> Self { /* collapse ranges to .start */ }
}
```

(Inspirado en T18-09 §6.1; T12-09; T15-02 §7; T20-01; T07-09 §6.1; T19-01 §4.1; T10-09.)

#### D.13.21. Discovery con abort si `>50% sketches fallan`

```rust
// src/discovery/loop.rs
pub fn should_abort_due_to_failures(state: &DiscoveryState) -> bool {
    state.failed_count * 2 > state.total_attempts
}
```

(Inspirado en T11-06.)

---

### D.14. §10 CLI (subcomandos y flags adicionales)

#### D.14.1. `moagan doctor`

```rust
// src/cli/doctor.rs
#[derive(Parser)]
pub struct Doctor {
    #[arg(long)] include_provider_ping: bool,
    #[arg(long)] include_storage_test: bool,
}
```

Imprime config efectiva, ping a provider, writability de SQLite y `.runs/`. (Inspirado en T01-09; T20-09 §3.1.)

#### D.14.2. `moagan diff <run_a> <run_b>`

```rust
// src/cli/diff.rs
pub struct Diff {
    pub run_a: Uuid,
    pub run_b: Uuid,
    #[arg(long)] json: bool,
    #[arg(long)] include_proposals: bool,
}
```

Reporta diff en params, artefactos, scores. (Inspirado en T01-10 §7.1; T16-01 §6.1; T10-08.)

#### D.14.3. `moagan repair`

```rust
// src/cli/repair.rs (nuevo)
pub struct Repair {
    #[arg(long)] cleanup_orphans: bool,
    #[arg(long)] reindex_artifacts: bool,
    #[arg(long)] recover_zombies: bool,
    #[arg(long)] yes: bool,
}
```

Escanea `*.tmp.<uuid>`, locks stale, runs con `status=running` sin heartbeat. **Nunca modifica contenido de artefactos**. (Inspirado en T08-03; T18-08; T19-07.)

#### D.14.4. `moagan validate <brief_path>`

```rust
// src/cli/validate.rs
pub struct Validate {
    pub brief_path: PathBuf,
    #[arg(long)] mode: Option<Mode>,
}
```

Valida un brief pre-existente contra constraints duras, retorna exit code. (Inspirado en T16-01 §6.1.)

#### D.14.5. `--runs-dir <path>` global

```rust
// src/cli/mod.rs (extender Cli)
#[arg(long, global = true, env = "MOAGAN_RUNS_DIR")]
runs_dir: Option<PathBuf>,
```

Permite runs aislados (tests) y portabilidad. (Inspirado en T08-02; T16-10; T14-07 §7.1.)

#### D.14.6. `--hash-algo blake3|sha256`

```rust
// src/cli/export.rs
#[arg(long, value_enum, default_value_t = HashAlgo::Sha256)]
hash_algo: HashAlgo,
```

Mantiene `SHA256SUMS` por default, opcional blake3. (Inspirado en T08-02; T15-04; T17-05.)

#### D.14.7. `--prompt -` (stdin)

```rust
// src/cli/run.rs
#[arg(long, default_value_t = false)]
prompt_from_stdin: bool,
```

Si `--prompt -`, lee de stdin hasta EOF. (Inspirado en T20-09; T02-06 §5.1.)

#### D.14.8. `--batch` shortcut

```rust
#[arg(long)]
batch: bool,
```

(Inspirado en T20-09 §3.3.)

#### D.14.9. `--rerun --continue-from-phase <name>`

```rust
// src/cli/rerun.rs
#[arg(long)]
continue_from_phase: Option<String>,
```

Salta fases previas. Registra `reused_phases: ["intake", "clarify", ...]` en manifest. (Inspirado en T04-07 §2.5.)

#### D.14.10. `--assume <json>` para batch

```rust
// src/cli/run.rs
#[arg(long)]
assume: Option<String>,
```

(Inspirado en T07-05 §1.3.)

#### D.14.11. `--no-synthesis` y `--allow-injection`

```rust
#[arg(long)] no_synthesis: bool,
#[arg(long)] allow_injection: bool,
```

(Inspirado en T07-05 §1.12; T04-07 §7.2.)

#### D.14.12. `--attach <path>` repetible

```rust
#[arg(long, value_name = "PATH")]
attach: Vec<PathBuf>,
```

Gate de tamaño total `> 50 MiB` requiere `--allow-oversized`. (Inspirado en T04-06 §6.1.)

#### D.14.13. `--structured-proposals`

```rust
#[arg(long)]
structured_proposals: bool,
```

Solo `deep` puede usarlo. (Inspirado en T08-02 §5.2.)

#### D.14.14. `--force-eval`

```rust
#[arg(long)]
force_eval: bool,
```

(Inspirado en T06-10 §7.2.)

#### D.14.15. `--context-summary` y `--context-full` mutuamente exclusivos

```rust
#[arg(long, conflicts_with = "context_full")]
context_summary: bool,
#[arg(long, conflicts_with = "context_summary")]
context_full: bool,
```

(Inspirado en T02-06 §5.1.)

#### D.14.16. `--max-sketches` user override

```rust
#[arg(long)]
max_sketches: Option<usize>,
```

(Inspirado en T14-05 §4.2; T00-07.)

#### D.14.17. `--batch-proposals=N`

```rust
#[arg(long, value_name = "N")]
batch_proposals: Option<usize>,
```

(Inspirado en T04-07 §6.5.)

#### D.14.18. `--budget-suffix` parsing (`1k`, `1.5M`, `2G`)

```rust
fn parse_budget(s: &str) -> Result<u64, ParseIntError> {
    let (num, mult) = if let Some(stripped) = s.strip_suffix('k') { (stripped, 1_000) }
        else if let Some(stripped) = s.strip_suffix('M') { (stripped, 1_000_000) }
        else if let Some(stripped) = s.strip_suffix('G') { (stripped, 1_000_000_000) }
        else { (s, 1) };
    num.parse::<u64>().map(|n| n * mult)
}
```

(Inspirado en T04-07 §1.2.)

#### D.14.19. `telemetry cleanup --vacuum`

```rust
// src/cli/telemetry_cmd.rs
Cleanup { #[arg(long)] vacuum: bool, ... }
```

`VACUUM` en SQLite. (Inspirado en T04-07 §3.5; T00-08 §1439.)

#### D.14.20. Mutuamente exclusivos `--switch-provider XOR --refine XOR --rerank`

```rust
#[arg(long, conflicts_with_all = ["refine", "rerank"])]
switch_provider: Option<String>,
#[arg(long, conflicts_with_all = ["switch_provider", "rerank"])]
refine: Option<String>,
#[arg(long, conflicts_with_all = ["switch_provider", "refine"])]
rerank: bool,
```

(Inspirado en T19-07 §11.3.)

#### D.14.21. `moagan inspect --json` (interfaz estable)

```rust
#[arg(long)]
json: bool,
```

(Inspirado en T06-01 §8.3.)

#### D.14.22. `--redact-rewrite` con `--yes`

Ver §D.8.4.

#### D.14.23. Output: `comfy-table` para tablas

```rust
use comfy_table::{Table, Cell, Row};
fn render_runs(runs: &[RunSummary]) -> String {
    let mut t = Table::new();
    t.set_header(vec!["run_id", "mode", "status", "tokens", "started_at"]);
    for r in runs { t.add_row(vec![r.id, r.mode, r.status, r.tokens.to_string(), r.started_at]); }
    t.to_string()
}
```

(Inspirado en T12-09 §6.2; T20-06 §6.3.)

---

### D.15. §11 Config (extensiones)

#### D.15.1. `Config::load()` con precedencia

```
defaults < config.toml < .moagan.toml < env vars < CLI flags
```

(Inspirado en T07-08 §1.4; T17-08 §12.5; T08-02 §3.1; T01-09 §1.4.)

#### D.15.2. Routing declarativo TOML

```toml
# ~/.config/moagan/routing.toml
[[rules]]
when = { risk = "high" }
then = { mode = "deep", decompose = true }

[[rules]]
when = { complexity = "simple", risk = "low", has_questions = false }
then = { mode = "fast" }
```

(Inspirado en T07-08; T16-09; T00-10 §3.20.)

#### D.15.3. `discover_runs_with_input_hash()` al arranque

```rust
// src/storage/sqlite.rs
pub fn find_runs_with_input_hash(&self, hash: &str) -> Result<Vec<Uuid>>;
```

(Inspirado en T03-04 §3.2; T01-04 §6.5; T19-09.)

#### D.15.4. `MOAGAN_RUNS_DIR` env var

Ya en T01-06 §0.3. Documentar precedencia.

#### D.15.5. `--max-parallelism <= 64` validation

```rust
// src/config.rs::validate
if parallelism.max > 64 {
    return Err(Error::Config(format!("max_parallelism={} exceeds hard cap 64", max)));
}
```

(Inspirado en T04-10 D15.)

#### D.15.6. `BatchPolicy` flags

```rust
pub struct BatchPolicy {
    pub auto_accept_clarify: bool,
    pub skip_final_checkpoint: bool,
    pub fail_on_blocking_ambiguity: bool,
    pub output_format: BatchOutputFormat,
    pub batch_proposals: Option<usize>,
    pub on_needs_input: NeedsInputAction,
}
```

(Inspirado en T01-06 §23; T20-10 §3.5; T04-07; T06-01 §8.3.)

#### D.15.7. `BatchOutputFormat::JsonStable | JsonLines`

Ya en T01-06 §23.

---

### D.16. §12 Errors (enriquecer)

#### D.16.1. Tipos de error adicionales (consolidar)

Ver §D.12.8, §D.12.9, §D.12.10, §D.12.11, §D.12.14.

#### D.16.2. `Display` que auto-redacta

```rust
// src/error.rs
impl std::fmt::Display for MoaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = format!("{self:?}");
        write!(f, "{}", redact::apply(&s))
    }
}
```

(Inspirado en T20-03 §17.1.)

#### D.16.3. Errores con `cause` chain

```rust
// src/error.rs
pub struct MoaError {
    pub variant: Variant,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
    pub backtrace: Option<std::backtrace::Backtrace>,
}
```

(Inspirado en T08-04 §14.1; T19-10 §14.2.)

---

### D.17. §13 Telemetry (enriquecer)

#### D.17.1. `TelemetryEvent` enum exhaustivo (15+ variantes)

```rust
// src/telemetry/event.rs
pub enum TelemetryEvent {
    RunStart { run_id: Uuid, mode: Mode, at: Instant },
    RunEnd { run_id: Uuid, status: RunStatus, at: Instant },
    RunPause { run_id: Uuid, reason: PauseReason, at: Instant },
    RunResume { run_id: Uuid, at: Instant },
    PhaseStart { run_id: Uuid, phase: String, at: Instant },
    PhaseEnd { run_id: Uuid, phase: String, duration: Duration, at: Instant },
    CallStart { run_id: Uuid, call_id: Uuid, role: RoleId, at: Instant },
    CallEnd { run_id: Uuid, call_id: Uuid, usage: Usage, status: CallStatus, at: Instant },
    ProviderSwitch { run_id: Uuid, from: ProviderId, to: ProviderId, at: Instant },
    BudgetUpdate { run_id: Uuid, phase: String, used: u64, limit: u64 },
    CacheHit { run_id: Uuid, call_id: Uuid, key: String },
    CacheMiss { run_id: Uuid, call_id: Uuid, key: String },
    ValidationStart { run_id: Uuid, proposal_id: Uuid, at: Instant },
    ValidationEnd { run_id: Uuid, proposal_id: Uuid, status: ValidationStatus, at: Instant },
    SandboxIssue { run_id: Uuid, sandbox_id: Uuid, kind: SandboxError },
    Saturation { phase: String, requested: usize, granted: usize, at: Instant },
    DiscoverySaturated { run_id: Uuid, at: Instant },
    HumanCheckpoint { run_id: Uuid, ckp_id: Uuid, kind: CheckpointKind, at: Instant },
    Heartbeat { run_id: Uuid, parallel_in_use: usize, at: Instant },
}
```

(Inspirado en T18-06 §8.1; T05-07 §12.1; T19-06.)

#### D.17.2. `TelemetryHub` con sinks

```rust
// src/telemetry/mod.rs
pub struct TelemetryHub {
    pub run_file_sink: Option<FileSink>,
    pub file_sink: FileSink,
    pub sqlite: Db,
    pub level: TelemetryLevel,
}
impl TelemetryHub {
    pub fn emit(&self, ev: TelemetryEvent) -> Result<()>;
}
```

(Inspirado en T00-05 §11.2; T10-08 §8.)

#### D.17.3. Telemetry level

```rust
pub enum TelemetryLevel {
    Off,
    Aggregate,
    Full,
}
```

(Inspirado en T01-06 §11.4 + T06-08 §11.1; T16-09 §6.3.)

#### D.17.4. Daily log rotation

```rust
use tracing_appender::rolling::{daily, Rotation};
let file_appender = daily("/var/log/moagan", "moagan.log");
tracing_subscriber::fmt().with_writer(file_appender).json().init();
```

(Inspirado en T05-02 §17.1; T11-01.)

#### D.17.5. Heartbeat en background

```rust
// src/telemetry/heartbeat.rs (nuevo)
pub async fn heartbeat_loop(db: Db, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let active = db.active_runs().unwrap_or_default();
                for run in active {
                    db.update_heartbeat(&run).ok();
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}
```

(Inspirado en T20-05; T08-03.)

#### D.17.6. Recovery on startup (zombie runs)

```rust
// src/telemetry/recover.rs
pub fn recover_zombies(db: &Db) -> Result<Vec<Uuid>> {
    let threshold = chrono::Utc::now() - chrono::Duration::minutes(5);
    let zombies = db.query("SELECT run_id FROM runs WHERE status = 'running' AND last_heartbeat < ?", params![threshold])?;
    for run_id in &zombies {
        db.update_run_status(run_id, "failed", Some("zombie_recovered"))?;
    }
    Ok(zombies)
}
```

(Inspirado en T20-05; T11-04; T08-03; T18-08.)

#### D.17.7. CSV summary `sketches_summary.csv`

```rust
// src/telemetry/csv_summary.rs
pub fn write_sketches_summary(run_id: Uuid, root: &Path) -> Result<()> {
    let sketches = read_dir(root.join("sketches"))?;
    let mut w = csv::Writer::from_path(root.join("sketches_summary.csv"))?;
    w.write_record(&["id", "created_at", "gate", "tags", "cluster", "category", "model", "tokens", "output_hash"])?;
}
```

(Inspirado en T04-10; T18-04.)

#### D.17.8. `dashboard.html` self-contained

```rust
// src/telemetry/dashboard_static.rs
pub fn render_dashboard_static(db: &Db, run_id: Uuid, dest: &Path) -> Result<()> {
    let data = db.collect_run_data(run_id)?;
    let html = format!(r#"<!DOCTYPE html><html>...{data_inline_json}...</html>"#);
    fs::write(dest, html)?;
    Ok(())
}
```

(Inspirado en T19-07 §9.6; T00-05 D19.)

#### D.17.9. `tracing::filter` específico para moagan

```rust
tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,moagan=debug,moagan::telemetry=trace")))
    .json()
    .init();
```

(Inspirado en T16-04 §17.1.)

#### D.17.10. `phase!` macro para `info_span!`

```rust
// src/telemetry/macros.rs
#[macro_export]
macro_rules! phase {
    ($name:expr) => { tracing::info_span!("phase", name = $name) };
}
```

(Inspirado en T05-02 §17.2.)

#### D.17.11. `ReportingLayer` con redact-on-write

Ver §D.8.3.

---

### D.18. §14 Tests (enriquecer)

#### D.18.1. `cargo test` con profile normal y release

```yaml
# .github/workflows/test.yml
test:
  strategy:
    matrix:
      profile: [dev, release]
```

(Inspirado en T05-09 §15.4.)

#### D.18.2. `proptest` para hashes y serialización

```rust
use proptest::prelude::*;
proptest! {
    #[test]
    fn hash_invariant(s: String) {
        let h1 = CallKey::hash(&s, "test", "1.0", "minimax", "M3", 0.0, 1.0, 1024);
        let h2 = CallKey::hash(&s, "test", "1.0", "minimax", "M3", 0.0, 1.0, 1024);
        prop_assert_eq!(h1, h2);
    }
}
```

(Inspirado en T00-05 D20.)

#### D.18.3. `assert_cmd` para CLI tests

```rust
use assert_cmd::Command;
#[test]
fn doctor_runs() {
    let mut cmd = Command::cargo_bin("moagan").unwrap();
    cmd.arg("doctor").assert().success();
}
```

(Inspirado en T09-07 §29.1.)

#### D.18.4. `insta` snapshots para JSON output

```rust
use insta::assert_yaml_snapshot;
#[test]
fn manifest_format() {
    let manifest = Manifest::sample();
    assert_yaml_snapshot!(manifest);
}
```

(Inspirado en T00-05 D20 + baseline.)

#### D.18.5. Mock provider determinístico

Ya en T01-06 §14.4. Mantener.

---

### D.19. §15 / §26 Provider (enriquecer)

#### D.19.1. `Provider::Family` para switch cache-preserving

```rust
// src/llm/provider.rs
pub enum Family { Anthropic, OpenAI, Custom }
pub trait Provider: Send + Sync {
    fn family(&self) -> Family;
    fn endpoint(&self) -> &str;
    fn model(&self) -> &str;
    fn supports_token_plan(&self) -> bool;
    fn supports_vision(&self) -> bool { false }
    fn supports_streaming(&self) -> bool { false }
    fn max_context_tokens(&self) -> u32 { 200_000 }
    fn max_output_tokens(&self) -> u32 { 8_192 }
    fn default_temperature_tagger(&self) -> f32 { 0.1 }
    fn default_temperature_extractor(&self) -> f32 { 0.0 }
    fn current_usage(&self) -> Usage;
    fn update_usage(&self, usage: Usage);
    async fn send(&self, req: &Request) -> Result<Response>;
    async fn stream(&self, req: &Request) -> Result<StreamingResponse> { Err(Error::StreamingNotSupported) }
    async fn fetch_plan(&self) -> Result<PlanSnapshot> { Err(Error::NotSupported) }
    async fn identity(&self) -> Result<ModelIdentity>;
}
pub fn provider_compatible(from: &dyn Provider, to: &dyn Provider) -> bool {
    from.family() == to.family()
        && url_host(from.endpoint()) == url_host(to.endpoint())
}
```

(Inspirado en T01-10 §3.1; T18-09 §5.0; T18-06 §0.7; T09-08 §9.5; T09-02; T06-09 §5.4; T04-04 §12.1.)

#### D.19.2. `Capabilities` para capacidades

```rust
pub struct Capabilities {
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub supports_temperature: bool,
    pub supports_top_p: bool,
    pub supports_tools: bool,
    pub supports_json_mode: bool,
    pub supports_vision: bool,
    pub supports_streaming: bool,
    pub family: Family,
    pub endpoint_host: String,
    pub model_version: String,
}
```

(Inspirado en T18-09 §5.0; T18-06 §0.7; T01-10 §9.1.)

#### D.19.3. Per-provider config (multi-instancia)

```toml
# config.toml
[providers.minimax_main]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
api_key_ref = "env:MINIMAX_API_KEY"
plan_id = "weekly"

[providers.minimax_backup]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
api_key_ref = "env:MINIMAX_API_KEY_BACKUP"
plan_id = "monthly"
```

(Inspirado en T08-01 §6.2; T02-07 §2.1; T08-03 §5.8.)

#### D.19.4. `PlanTracker` con `state: parking_lot::Mutex<HashMap<PlanId, PlanState>>`

```rust
// src/llm/plan.rs
pub struct PlanTracker {
    pub state: parking_lot::Mutex<HashMap<PlanId, PlanState>>,
    pub plan_type: String,
    pub plan_limit: u64,
    pub warning_threshold: f32,
    pub hard_limit: f32,
    pub reset_at: Option<DateTime<Utc>>,
    pub used: AtomicU64,
}
```

(Inspirado en T20-01; T18-06 §0.7; T00-08 §1542.)

#### D.19.5. Per-provider circuit breaker

```rust
// src/llm/circuit_breaker.rs (nuevo)
pub struct CircuitBreaker {
    threshold: u32,
    window: Duration,
    open_until: AtomicU64,
    consecutive_failures: AtomicU32,
}
impl CircuitBreaker {
    pub fn record_success(&self);
    pub fn record_failure(&self);
    pub fn is_open(&self) -> bool;
}
```

(Inspirado en T00-08 §1428-1435; T08-03 §5.8; T00-09; T03-03.)

#### D.19.6. Per-provider rate limiter (token bucket)

Ver §D.9.7.

#### D.19.7. Streaming response con TTFT

```rust
// src/llm/response.rs
pub struct StreamingResponse {
    pub first_chunk_at: Instant,
    pub chunks: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    pub total_bytes: Arc<AtomicUsize>,
    pub truncation_detected: Arc<AtomicBool>,
}
```

(Inspirado en T20-07 §6.4; T09-08 §5.6.)

#### D.19.8. `Provider::fetch_plan` para plan monitoring proactivo

```rust
async fn fetch_plan(&self) -> Result<PlanSnapshot> {
    let url = format!("{}/v1/usage", self.endpoint.trim_end_matches("/messages"));
    let resp = self.client.get(&url)
        .header("x-api-key", &self.api_key)
        .header("anthropic-version", "2023-06-01")
        .send().await?;
}
```

(Inspirado en T04-04 §12.1; T06-04; T19-01; T18-09.)

#### D.19.9. `ApiKey::from_ref` parser

```rust
// src/llm/api_key.rs
pub enum ApiKeySource { Interactive, Env(String), File(PathBuf), Literal(String), Keyring(String) }
pub struct ApiKey { source: ApiKeySource, value: SecretString }
impl ApiKey {
    pub fn from_ref(r: &str) -> Result<Self> {
        if let Some(var) = r.strip_prefix("env:") { Ok(Self { source: ApiKeySource::Env(var.into()), value: SecretString::new("") }) }
        else if let Some(path) = r.strip_prefix("file:") { /* read file */ }
        else if let Some(lit) = r.strip_prefix("literal:") { /* error si !privacy.allow_literal */ }
        else { Ok(Self { source: ApiKeySource::Interactive, value: SecretString::new("") }) }
    }
    pub fn redacted(&self) -> String;
    pub fn into_header(&self) -> String;
}
```

(Inspirado en T20-06 §6.3; T17-04 §8.3; T01-08 §5.6; T15-01 §5.5.)

#### D.19.10. `PlanLimit::Window` enum

```rust
pub enum PlanWindow { Weekly, Monthly, Custom, Unlimited { resets_at: DateTime<Utc> } }
```

(Inspirado en T19-04; T15-01.)

#### D.19.11. `provider_version: String` para cache invalidation

```rust
// en CallKey o Provider struct
pub provider_version: String,
```

(Inspirado en T11-02; T01-10.)

#### D.19.12. `cached_tokens` y `reasoning_tokens` opcionales

```rust
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: Option<u32>,
    pub cached_output_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub total_tokens: u32,
}
```

(Inspirado en T09-07 §5.3; T00-09.)

#### D.19.13. `AnthropicCompatProvider` body builder

```rust
pub fn build_anthropic_body(req: &Request, model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": req.prompt}],
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "top_p": req.top_p,
        "system": req.system_prompt,
        "stop_sequences": req.stop_sequences,
        "metadata": { "user_id": req.user_id }
    })
}
```

(Inspirado en T01-08 §4.2; T06-09 §7.3; T00-10 §3.6.2.)

#### D.19.14. Wire format docs

```rust
// Anthropic-compat: POST {endpoint}/v1/messages
//   headers: x-api-key, anthropic-version, content-type
// OpenAI-compat:   POST {endpoint}/v1/chat/completions
//   headers: Authorization: Bearer, content-type
```

Endpoints concretos por provider:

| Provider | Family | Endpoint |
|---|---|---|
| `minimax` | Anthropic | `https://api.minimax.io/anthropic/v1/messages` |
| `glm` | OpenAI | `https://api.z.ai/api/paas/v4/chat/completions` |
| `qwen` | OpenAI | `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions` |
| `kimi` | OpenAI | `https://api.moonshot.ai/v1/chat/completions` |
| `deepseek` | OpenAI | `https://api.deepseek.com/v1/chat/completions` |
| `opencode_go` | Custom | configurable |

(Inspirado en T06-09 §7.3; T00-10 §3.6.3.)

#### D.19.15. Cache markers por provider

```rust
// Anthropic-compat
"cache_control": { "type": "ephemeral" }
// OpenAI-compat
"X-Cache-Key": "<hash>"
```

Si el provider no lo soporta, ignorar. (Inspirado en T06-09 §5.4.)

#### D.19.16. `MockResponse::match` por `(role, phase)`

Ya en T01-06 §26.3.

#### D.19.17. `MockResponse::http_status` y `body`

Ya en T01-06 §26.3.

#### D.19.18. `MockExhausted` error y `MockNoMatch`

Ya en T01-06 §26.3.

#### D.19.19. `ProviderPool` con `round_robin` y `health`

Ver §D.9.6.

#### D.19.20. `ProviderRegistry::pick(allow_paused: bool)`

```rust
// src/llm/registry.rs
pub fn pick(&self, allow_paused: bool) -> Arc<dyn Provider> {
    let rotation = self.rotation.lock();
    let provider = self.by_name[&rotation[rotation.cursor]];
    if !allow_paused && provider.plan_state() == PlanState::Paused {
        return self.pick_healthiest();
    }
    provider
}
```

(Inspirado en T08-03 §5.8.)

#### D.19.21. `BatchRL` decision tree

```rust
pub enum BatchRateLimitDecision {
    Recoverable { backoff: Duration },
    FallbackTo { provider: ProviderId },
    AbortRun { reason: String },
}
```

(Inspirado en T04-04 §15.1.)

#### D.19.22. `StopReason` enum

```rust
pub enum StopReason {
    EndTurn,
    MaxTokens,
    Stop { sequence: String },
    ToolUse,
    Error { code: String, msg: String },
}
```

Persistir en `calls.finish_reason`. (Inspirado en T00-05; T09-02.)

#### D.19.23. `time-period-aware` EMA estimator para tokens

```rust
// src/llm/cost_estimator.rs
pub struct CostEstimator {
    alpha: f32,
    state: parking_lot::Mutex<HashMap<(ProviderId, ModelId), EmaState>>,
}
impl CostEstimator {
    pub fn observe(&self, provider: ProviderId, model: ModelId, tokens: u64);
    pub fn estimate(&self, provider: ProviderId, model: ModelId) -> u64;
}
```

(Inspirado en T04-02; T06-05.)

#### D.19.24. `ApiKey::Literal` solo con `privacy.allow_literal=true`

```rust
// src/config.rs::validate
if api_key_ref.starts_with("literal:") && !cfg.privacy.allow_literal {
    return Err(Error::Config("literal api key requires privacy.allow_literal=true".into()));
}
```

(Inspirado en T20-02 §11.2.)

---

### D.20. §16 Phase implementations (refinamientos)

#### D.20.1. `intake` con `injection_safety_state`

```rust
// src/phases/intake.rs
pub struct InjectionSafetyState { risk: RiskLevel, matches: Vec<(PatternId, String)> }
pub enum RiskLevel { Safe, Suspicious, Blocked }
```

(Inspirado en T17-10 §4.1; T20-10 §4.1; T18-10 §7.1; T00-03.)

#### D.20.2. `intake` con BOM removal + NFC

```rust
// src/ingest/normalize.rs
pub fn normalize(raw: &str) -> String {
    let no_bom = raw.trim_start_matches('\u{FEFF}');
    let nfc: String = no_bom.nfc().collect();
    nfc
}
```

(Inspirado en T20-10 §4.1; T00-08.)

#### D.20.3. `intake` con detector de injection

```rust
const INJECTION_PATTERNS: &[&str] = &[
    "(?i)ignore previous instructions",
    "(?i)ignore all prior",
    "(?i)system:",
    "(?i)assistant:",
    "(?i)<\\|im_start\\|>",
    "(?i)you are now",
    "(?i)forget everything",
];
```

(Inspirado en T17-10 §4.1; T00-03; T20-10.)

#### D.20.4. `intake` con tamaño máximo 256 KiB

```rust
pub const MAX_NORMALIZED_INPUT_BYTES: usize = 256 * 1024;
if normalized.len() > MAX_NORMALIZED_INPUT_BYTES { return Err(Error::InputTooLarge); }
```

(Inspirado en T20-10 §4.2.)

#### D.20.5. `intake` con `HostilePrompt` blocking

```rust
if state.risk == RiskLevel::Blocked && !cli.allow_injection {
    return Err(Error::HostilePrompt(state.matches));
}
```

(Inspirado en T20-10; T18-04.)

#### D.20.6. `route` con declarative routing

```rust
// src/phases/route.rs
pub fn route(brief: &CanonicalBrief, policy: &ExecutionPolicy) -> Result<Mode> {
    if let Some(m) = policy.mode { return Ok(m); }
    let rules = load_routing_rules()?;
    for rule in &rules {
        if rule.matches(brief) { return Ok(rule.mode()); }
    }
    // fallback: LLM router
}
```

(Inspirado en T07-08; T16-09; T00-10 §3.20.)

#### D.20.7. `gate` con checks tipados

```rust
// src/validators/structural.rs
pub struct Gate {
    pub checks: Vec<Box<dyn GateCheck>>,
}
pub trait GateCheck {
    fn name(&self) -> &str;
    fn check(&self, p: &Proposal) -> GateResult;
}
pub enum GateResult { Pass, Warn { reason: String }, Fail { reason: String } }
```

(Inspirado en T03-06 §811; T18-06 §0; T05-03 §8.7.)

---

### D.21. §17 Cardinality (enriquecer)

#### D.21.1. Cardinalidad por modo (tabla concreta)

| Modo | sketches | proposals | critics/proposal | judges | repair rounds |
|---|---:|---:|---:|---:|---:|
| `fast` | 0 | 2 | 1 | 1 | 0 |
| `standard` | 4 | 3 | 2 | 3 | 1 |
| `deep` | 5 | 5 | 2 | 3 | 2 |
| `explore` | 10 | 0 | 0 | 0 | 0 |
| `batch` | configurable | configurable | configurable | configurable | configurable |
| `discovery` | 40..=500 | 1..=10 | 0 | 0 | 0 |

(Inspirado en T15-02 §7; T18-09 §6.1; T06-02 §7.2; T19-01 §4.1; T20-01.)

#### D.21.2. `Cardinality::for_mode` con `Range<usize>`

Ver §D.13.20.

#### D.21.3. `SelectionPlan` con `keep_top / keep_diverse / keep_outlier`

Ver §D.12.4.

#### D.21.4. Per-phase cost table

```rust
pub fn cost_per_phase(role: &RoleId) -> (u64, u64) {  // (input_avg, output_avg)
    match role.as_str() {
        "intake"     => (500, 200),
        "clarify"    => (1500, 800),
        "decomposer" => (3000, 2500),
        "sketcher"   => (1500, 600),
        "proposer"   => (4000, 3000),
        "critic_*"   => (5000, 1500),
        "judge_*"    => (4000, 800),
        "adversary"  => (4000, 1500),
        "repairer"   => (5000, 3500),
        "tagger"     => (2000, 200),
        "facet_deriver" => (2000, 500),
        "extractor"  => (3000, 2000),
        "integrator" => (4000, 3000),
        "refiner"    => (4000, 2500),
        _            => (1000, 500),
    }
}
```

(Inspirado en T09-10 §3.4; T05-07 §2.5.)

#### D.21.5. Budget cascade en caso de insuficiencia

```rust
// src/execution/budget_cascade.rs
pub fn apply_budget_pressure(state: &mut RunState, deficit: u64) -> BudgetAction {
    if state.skip_optional_phases { BudgetAction::SkipOptional }
    else if state.reduce_cardinality { BudgetAction::ReduceCardinality(deficit / 100) }
    else if state.borrow_later { BudgetAction::BorrowFromLater { phase: state.later_with_surplus() } }
    else { BudgetAction::Abort }
}
```

(Inspirado en T02-04 D3; T04-10; T03-05 §4.1.)

#### D.21.6. Per-mode retry budget matrix

[partial — module shipped, wire-up pending] Módulo en src/llm/retry_budget.rs (PR #29, sub-fase K). Wire-up al retry loop en phases/phase.rs::call_with_retry_parse se aplaza a PR #52 (sub-fase Q2). Anotación propuesta en docs/q1-v0.3-status-sync.

| Mode | transport | rate-limit | parse | schema | timeout | truncated |
|---|---:|---:|---:|---:|---:|---:|
| `fast` | 1 | 1 | 1 (json_repair) | 1 (json_repair) | 1 | 1 |
| `standard` | 2 | 2 | 1 (json_repair) | 1 (json_repair) | 1 | 1 |
| `deep` | 2 | 3 | 2 (json_repair) | 2 (json_repair) | 1 | 1 |
| `explore` | 1 | 1 | 1 (json_repair) | 1 (json_repair) | 1 | 1 |
| `batch` | 1 | 1 | 1 (json_repair) | 1 (json_repair) | 1 | 1 |
| `discovery` | 1 | 1 | 0 | 0 | 1 | 0 |

(Inspirado en T16-06 §2.5.)

> **Implementación v0.4 — sub-fase K.9 (commit `861c660`,
> agrupado con K.2).** `src/llm/retry_budget.rs` ships
> `pub enum RetryReason { Transport, RateLimit, Parse, Schema,
> Timeout, Truncated }`, `pub struct RetryBudget { max_attempts,
> use_json_repair }`, y la función pura `pub fn budget_for(mode,
> reason) -> RetryBudget` que reproduce la matriz del spec
> verbatim. `phase.rs::call_with_retry_parse` mantiene su
> `n_attempts = 5` hard-coded por ahora; el siguiente paso de
> integración (sub-fase L o un sub-fase K.10) substituirá la
> constante por `budget_for(current_mode, current_reason)`. 10
> unit tests pinea los valores del spec (Deep+Parse=2 con
> json_repair, Deep+RateLimit=3, Fast=1 siempre, Standard+Parse=1
> con json_repair, etc.).

#### D.21.7. Quorum de judges por modo (D40)

Ver D.3 tabla.

#### D.21.8. `Cardinality::for_mode_default` para cardinalidad "soft" y "hard"

```rust
pub struct CardinalityPolicy {
    pub default: Cardinality,
    pub soft_ceiling: Cardinality,    // warning if exceeded
    pub hard_ceiling: Cardinality,    // error if exceeded
}
```

(Inspirado en T18-04; T12-09; T14-05.)

---

### D.22. §18 Adversary / iter (mejoras)

#### D.22.1. Adversary con 5 patrones

Ver §D.12.5.

#### D.22.2. `RefineAction` enum con 7 variantes

Ya en T01-06 §18.2. Mantener.

#### D.22.3. `invalidate_downstream` con DAG traversal

```rust
fn invalidate_downstream(p: &Proposal, action: &RefineAction, graph: &ArtifactGraph) -> Vec<PathBuf> {
    let mut to_delete = vec![];
    match action {
        RefineAction::Focus { proposal_id, .. } => {
            // BFS desde p en artifact graph, eliminar descendientes
            let descendants = graph.descendants(&format!("proposals/p_{}.json", proposal_id));
            to_delete.extend(descendants);
        }
        // ...
    }
    to_delete
}
```

(Inspirado en T18-06 §8.2; T06-08 §8.2.)

#### D.22.4. `SynthesisRequest` con `prohibited_decisions`

```rust
pub struct SynthesisRequest {
    pub source_proposals: Vec<ProposalId>,
    pub invariants: Vec<String>,
    pub prohibited_decisions: Vec<String>,    // ej: ["monolith", "sync_rpc"]
}
```

(Inspirado en T19-04 §14.8.)

#### D.22.5. `StaleArtifact` log

```rust
pub struct StaleArtifact {
    pub artifact_path: PathBuf,
    pub invalidated_by: String,    // propuesta o acción
    pub reason: String,
    pub at: DateTime<Utc>,
}
```

Persistir en `phases.jsonl` con `event='invalidate'`. (Inspirado en T19-04 §14.6.)

---

### D.23. §20 Parallelism runtime (refinamientos)

#### D.23.1. `Parallelism::acquire_many_owned`

Ver §D.9.1.

#### D.23.2. Backpressure on semaphore starvation

Ver §D.9.2.

#### D.23.3. `SaturationEvent` con `requested` y `granted`

Ver §D.9.5.

#### D.23.4. `concurrent_peak` metric cada 5s

```rust
// src/telemetry/metrics.rs
pub async fn metrics_loop(parallelism: Arc<Parallelism>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let peak = parallelism.in_use_peak();
        metrics::gauge!("moagan_parallelism_in_use", peak as f64);
    }
}
```

(Inspirado en T00-03 §701-715.)

#### D.23.5. `ParallelismPool::acquire(n) -> Vec<OwnedSemaphorePermit>`

Ver §D.9.1.

---

### D.24. §22 Migrations (extensiones)

#### D.24.1. `v002_extended.sql` con tablas adicionales

Ver §D.5.1.

#### D.24.2. Triggers

```sql
-- trg_run_status_change
-- trg_run_heartbeat
```

Ver §D.5.3.

#### D.24.3. Migraciones incrementales sin breaking changes

```rust
const MIGRATIONS: &[(&str, &str)] = &[
    ("v001_initial.sql", include_str!("migrations/v001_initial.sql")),
    ("v002_extended.sql", include_str!("migrations/v002_extended.sql")),
];
```

(Inspirado en T01-06 §22 + T17-05 §507.)

---

### D.25. §24 Sandbox tests (extensiones)

#### D.25.1. `test_rust_validator` con `cargo check`

Ya en T01-06 §24.

#### D.25.2. `test_python_validator` con `pip install --user`

```rust
#[tokio::test]
async fn test_python_validator() {
    let sandbox = Sandbox::new(Duration::from_secs(30), vec!["python3".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("main.py"), "print('hi')").unwrap();
    let result = sandbox.run("python3", &["main.py"]).await.unwrap();
    assert_eq!(result.exit_code, 0);
}
```

(Inspirado en T01-06 §24 + T00-06 §11.2.)

#### D.25.3. `test_typescript_validator` con `tsc --noEmit`

```rust
#[tokio::test]
async fn test_typescript_validator() {
    let sandbox = Sandbox::new(Duration::from_secs(60), vec!["tsc".into(), "node".into()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("hello.ts"), "console.log('hi')").unwrap();
    fs::write(dir.path().join("tsconfig.json"), r#"{"compilerOptions":{"target":"es2020"}}"#).unwrap();
    let result = sandbox.run("tsc", &["--noEmit"]).await.unwrap();
    assert_eq!(result.exit_code, 0);
}
```

(Inspirado en T00-06 §11.3; T08-10 §10.1.)

#### D.25.4. `test_sandbox_deny_network`

```rust
#[tokio::test]
async fn test_sandbox_deny_network() {
    let sandbox = Sandbox::new(Duration::from_secs(5), vec!["python3".into()]).unwrap();
    let result = sandbox.run("python3", &["-c", "import urllib.request; urllib.request.urlopen('https://example.com')"]).await;
    assert!(result.is_err() || !result.unwrap().stdout.contains("200"));
}
```

(Inspirado en T18-06 §3.3; T07-05 §1.7.)

---

### D.26. §25 Error codes (extensiones)

#### D.26.1. Tabla completa de error codes

Ver §D.12.8.

#### D.26.2. `classify_error` con tabla

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

(Inspirado en T01-06 §25 + extensiones en T19-07 §4.1; T20-08 §7.4.)

#### D.26.3. `TRANSPORT_ERROR` con diagnóstico

```rust
match err {
    reqwest::Error if err.is_timeout() => "TRANSPORT_TIMEOUT",
    reqwest::Error if err.is_connect() => "TRANSPORT_CONNECT",
    reqwest::Error if err.is_request() => "TRANSPORT_REQUEST",
    _ => "TRANSPORT_OTHER",
}
```

(Inspirado en T20-08 §7.4.)

#### D.26.4. Stable `ErrorCode` con `is_retriable` y `is_circuit_opening`

```rust
impl ErrorCode {
    pub fn is_retriable(&self) -> bool {
        matches!(self, Self::Http429 | Self::Http500 | Self::Http502 | Self::Http503 | Self::Http504
            | Self::TransportError | Self::TimeoutSketch | Self::ProviderOverloaded)
    }
    pub fn is_circuit_opening(&self) -> bool {
        matches!(self, Self::Http429 | Self::Http500 | Self::Http502 | Self::Http503 | Self::Http504
            | Self::TransportError | Self::ProviderOverloaded | Self::Auth { .. })
    }
}
```

(Inspirado en T15-01 §0.4.)

#### D.26.5. Salidas con códigos estructurados (JSON)

```json
{
  "error_code": "HTTP_429",
  "error_message_redacted": "Rate limit exceeded",
  "retry_after_seconds": 60,
  "retriable": true,
  "is_circuit_opening": true
}
```

(Inspirado en T15-01 §0.4; T20-06 §6.4.)

#### D.26.6. `Display` con redact automático

Ver §D.16.2.

---

### D.27. §27 Telemetry redact (extensiones)

#### D.27.1. `ReportingLayer` con redact

Ver §D.8.3.

#### D.27.2. `redact_audit` table

Ver §D.5.1.

#### D.27.3. `moagan telemetry cleanup --redact-rewrite`

Ver §D.8.4.

---

### D.28. §28 Reconciliación (extensiones)

#### D.28.1. `reconcile(run_id)` con scan de artefactos

```rust
pub fn reconcile(run_id: Uuid) -> Result<()> {
    let root = root_dir()?.join(".runs").join(run_id.to_string());
    let manifest = Manifest::load(&root)?;
    let sketches_on_disk = count_files(&root.join("sketches"))?;
    let sketches_in_db = db.count_sketches(run_id)?;
    if sketches_on_disk != sketches_in_db {
        db.reindex_sketches(run_id, &root)?;
    }
    // ... mismo para proposals, critiques, evaluations
    let jsonl_phases = read_phases_jsonl(&root.join("telemetry").join("phases.jsonl.gz"))?;
    let db_phases = db.get_phases(run_id)?;
    if jsonl_phases.len() != db_phases.len() {
        db.reindex_phases(run_id, jsonl_phases)?;
    }
    Ok(())
}
```

(Inspirado en T01-06 §28.1; ampliado.)

#### D.28.2. `moagan repair` subcomando

Ver §D.14.3.

#### D.28.3. `cleanup_orphans()` al arranque

```rust
pub fn cleanup_orphans(root: &Path) -> Result<usize> {
    let mut count = 0;
    for entry = walkdir(root) {
        if entry.path().extension() == Some("tmp") || entry.path().ends_with(".tmp.<uuid>") {
            fs::remove_file(entry.path())?;
            count += 1;
        }
    }
    Ok(count)
}
```

(Inspirado en T02-01 §2.5; T08-03 §15.3; T10-10; T19-10.)

#### D.28.4. `recover_zombies()` al arranque

Ver §D.17.6.

#### D.28.5. `reindex_artifacts()` regenera tabla `run_artifacts`

```rust
pub fn reindex_artifacts(run_id: Uuid, root: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in walkdir(root.join("sketches")) { /* hash, insert */ count += 1; }
    for entry in walkdir(root.join("proposals")) { /* hash, insert */ count += 1; }
    Ok(count)
}
```

(Inspirado en T01-06 §28.1 + T03-04 §3.2.)

---

### D.29. §29 Hardening (extensiones)

#### D.29.1. Validación de paths (canonicalize)

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

(Inspirado en T01-06 §29.1.)

#### D.29.2. Tamaño máximo de payload

```rust
pub const MAX_PROMPT_BYTES: usize = 250 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
```

(Inspirado en T01-06 §29.2.)

#### D.29.3. `tiktoken-rs` token estimation

```rust
pub fn estimate_tokens(text: &str) -> u64 {
    let bpe = cl100k_base().unwrap();
    bpe.encode_with_special_tokens(text).len() as u64
}
```

(Inspirado en T06-07 §13.2; T04-04; T13-04; reemplazando heurística de T01-06 §29.3.)

#### D.29.4. Strip control tokens de LLM

Ver §D.7.2.

#### D.29.5. Sanitize `cache_control` y `metadata` antes de send

```rust
// src/llm/request.rs
pub fn sanitize_request(req: &mut Request) {
    req.prompt = strip_control_tokens(&req.prompt);
    req.prompt = redact::apply(&req.prompt);
}
```

(Inspirado en T20-08 §7.6.)

#### D.29.6. `--allow-injection` flag

Ver §D.11.10.

#### D.29.7. Watchdog que mata árbol de procesos

Ver §D.11.11.

#### D.29.8. `SecretString` con `Drop=zeroize`

Ver §D.1.7.

#### D.29.9. `MoaganError::MoaganError::new(input, source)` chain

Ver §D.16.3.

---

### D.30. §30 Pipeline (refinamientos)

#### D.30.1. `Pipeline` struct con `Box<dyn PhaseObject>`

```rust
pub struct Pipeline {
    pub manifest: Manifest,
    pub db: Db,
    pub parallelism: Arc<Parallelism>,
    pub cancel: CancellationToken,
    pub phases: Vec<Box<dyn PhaseObject>>,
}
```

(Inspirado en T02-09 §5.6.)

#### D.30.2. `Phase::resume` separado de `execute`

```rust
#[async_trait]
pub trait Phase: Send + Sync {
    async fn execute(&self, ctx: &RunContext) -> Result<PhaseResult>;
    async fn resume(&self, ctx: &RunContext, checkpoint: &Checkpoint) -> Result<PhaseResult>;
}
```

(Inspirado en T04-08 §4.1.)

#### D.30.3. `Pipeline::run_mode(mode)` dispatch

```rust
impl Pipeline {
    pub async fn run_mode(&mut self, mode: Mode) -> Result<()> {
        match mode {
            Mode::Fast      => self.run_fast().await,
            Mode::Standard  => self.run_standard().await,
            Mode::Deep      => self.run_deep().await,
            Mode::Explore   => self.run_explore().await,
            Mode::Batch     => self.run_batch().await,
            Mode::Discovery => self.run_discovery().await,
        }
    }
}
```

(Inspirado en T08-03 §6.5; T01-10 §4.1-4.6.)

#### D.30.4. `Phase::cost_hint` para budgeting

Ver §D.9.3.

---

### D.31. §31 CLI struct (extensiones)

#### D.31.1. `moagan doctor` subcomando

Ver §D.14.1.

#### D.31.2. `moagan diff` subcomando

Ver §D.14.2.

#### D.31.3. `moagan repair` subcomando

Ver §D.14.3.

#### D.31.4. `moagan validate` subcomando

Ver §D.14.4.

---

### D.32. §32 Budget (extensiones)

#### D.32.1. `Budget` con `consume(slot, tokens)` y errores tipados

```rust
impl Budget {
    pub fn consume(&mut self, slot: &str, tokens: u64) -> Result<()> {
        if self.total < tokens {
            return Err(Error::BudgetExhausted { phase: slot.into(), allocated: self.total, spent: 0 });
        }
        // ...
    }
}
```

(Inspirado en T01-06 §32 + D.12.9 para variant tipado.)

#### D.32.2. `BudgetWallet::consume(phase, amount)`

```rust
pub struct BudgetWallet { inner: parking_lot::Mutex<Budget> }
impl BudgetWallet {
    pub fn consume(&self, phase: &str, amount: u64) -> Result<()>;
    pub fn remaining(&self) -> u64;
}
```

(Inspirado en T03-07 §618; T07-10 §1541-1558.)

#### D.32.3. Cascade policy

Ver §D.21.5.

#### D.32.4. `BudgetController::try_consume(phase, tokens, calls)`

```rust
pub fn try_consume(&self, phase: &str, tokens: u64, calls: u32) -> Result<(), BudgetError> {
    let mut b = self.inner.lock();
    if b.tokens < tokens || b.calls < calls {
        return Err(BudgetError::Exhausted { phase: phase.into(), consumed_tokens: b.tokens });
    }
    b.tokens -= tokens;
    b.calls -= calls;
    Ok(())
}
```

(Inspirado en T01-02 §6.2; T06-02 §4.4; T07-10.)

#### D.32.5. `Budget` por-fase allocation

```rust
pub struct Budget {
    pub total_tokens: u64,
    pub per_phase: HashMap<PhaseKind, u64>,
    pub timeout_sketch_s: Duration,
    pub timeout_phase_s: Duration,
    pub timeout_total_s: Duration,
    pub cardinality: Cardinality,
}
```

(Inspirado en T19-01 §0.4.2; T05-07 §2.6; T15-02 §6.1.)

---

### D.33. §33 Manifest (extensiones)

#### D.33.1. `manifest.json` con `config_snapshot` y `hash_algo`

```json
{
  "schema_version": "v1",
  "run_id": "018f3a2b-...",
  "mode": "standard",
  "status": "running",
  "client": { "cli_version": "0.4.0", "os": "linux", "arch": "x86_64" },
  "execution_policy": {
    "timeouts": { "sketch": 120, "phase": 0, "total": 0 },
    "parallelism": { "sketch": 4, "phase": 4, "extraction": 4, "max": 4 },
    "interactive": true,
    "router": "auto"
  },
  "budget": { "tokens_total": 0, "tokens_used": 0, "by_slot": {} },
  "provider": { "current": "minimax", "plan_id": "weekly", "api_key_ref": "env:MINIMAX_API_KEY" },
  "provider_changes": [],
  "models_used": { "minimax": { "calls": 0, "tokens": 0, "errors": 0 } },
  "discovery": { "matrix_cardinality": 240, "categories": 12, "uncategorized": 47 },
  "deliverables": { "final_dir": "final", "files": [] },
  "lineage_paths": { "relative": {}, "absolute": {} },
  "hashes": { "manifest.json": "sha256:..." },
  "config_snapshot": { /* Config completo serializado */ },
  "hash_algo": "sha256",
  "shared_brief_hash": null,
  "duplicates_with": [],
  "reused_phases": [],
  "cancel_token_id": "...",
  "warnings": ["timeout_total=0 means infinite"]
}
```

(Inspirado en T01-06 §33 + T17-05 §271; T01-04 §6.3; T15-04 §7.6; T04-07 §2.5; T13-01 §6.2.)

#### D.33.2. `manifest.history[]` con cambios de provider

```json
{
  "history": [
    { "at": "...", "event": "status_change", "from": "running", "to": "paused", "reason": "plan_exceeded" },
    { "at": "...", "event": "provider_change", "from": "minimax", "to": "minimax_backup" }
  ]
}
```

(Inspirado en T10-04 §7.3.)

#### D.33.3. `manifest.alerts[]` para warnings

```json
{
  "alerts": [
    { "level": "warning", "code": "BUDGET_50_PERCENT", "at": "...", "details": "..." }
  ]
}
```

(Inspirado en T01-06 §33.)

#### D.33.4. `MANIFEST.txt` human-readable (export)

```
Run: 018f3a2b-... (standard)
Mode: standard
Started: 2026-07-25T10:30:00Z
Ended:   2026-07-25T10:35:42Z
Status: completed
Tokens: 124000 (used) / 250000 (budget)
Provenance hashes: SHA256SUMS
Schema version: v1
```

(Inspirado en T19-07 §7.1.)

#### D.33.5. `manifest.schema_version: u32` con semver

```rust
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
```

(Inspirado en T01-06 §33 + T04-08 §4.4; T07-10 §727.)

#### D.33.6. `prompt_set_hash` y `config_hash` para reproducibilidad

```rust
pub prompt_set_hash: String,    // blake3 de todos los .toml de prompts
pub config_hash: String,        // blake3 del config.toml efectivo
```

(Inspirado en T16-06 §3.3; T09-08 §3.3.)

#### D.33.7. `tool_versions` field

```json
{
  "tool_versions": { "cargo": "1.97.1", "rustc": "1.97.1", "python3": "3.12.3", "node": "20.10.0" }
}
```

(Inspirado en T00-06 §11.4.)

#### D.33.8. `versioned_manifest.json` para versionado

(Inspirado en T15-05 §7.2; T19-10 §14.2.)

---

### D.34. §34 Discovery resilience (extensiones)

#### D.34.1. Reintentos parciales en sketch

Ya en T01-06 §34. Mantener.

#### D.34.2. `SketchLoopState` persistido

```rust
pub struct SketchLoopState {
    pub target: usize,
    pub hard_cap: usize,
    pub cola_reserva: usize,
    pub saturated_models: HashSet<ModelId>,
    pub start_time: Instant,
}
impl SketchLoopState {
    pub fn save(&self, run_id: Uuid) -> Result<()>;
    pub fn load(run_id: Uuid) -> Result<Option<Self>>;
}
```

(Inspirado en T04-05 §6.5.)

#### D.34.3. `fsync` por sketch antes de checkpoint

```rust
// src/discovery/loop.rs
pub async fn save_sketch(sk: &Sketch, root: &Path) -> Result<()> {
    let path = root.join(format!("sketches/sk_{}.json", sk.id));
    fs::write(&path, serde_json::to_string_pretty(sk)?)?;
    let f = File::open(&path)?;
    f.sync_all()?;
    Ok(())
}
```

(Inspirado en T04-05 §6.6.)

---

### D.35. §35 API key (extensiones)

#### D.35.1. `SecretString` con `Drop=zeroize`

Ver §D.1.7.

#### D.35.2. `ApiKey::from_ref` parser

Ver §D.19.9.

#### D.35.3. `api_keys.toml` precedence

Ver §D.12.13.

#### D.35.4. `ApiKey::Literal` solo con `privacy.allow_literal=true`

Ver §D.19.24.

#### D.35.5. API key resolution on first use

```rust
// src/llm/api_key.rs
impl ApiKey {
    pub fn resolve(&mut self) -> Result<()> {
        if self.value.is_empty() {
            self.value = match &self.source {
                ApiKeySource::Env(var) => std::env::var(var).map(SecretString::new)?,
                ApiKeySource::File(path) => fs::read_to_string(path).map(SecretString::new)?,
                ApiKeySource::Literal(s) => SecretString::new(s.clone()),
                ApiKeySource::Interactive => {
                    let key = rpassword::prompt_password("API key: ")?;
                    SecretString::new(key)
                },
                ApiKeySource::Keyring(alias) => keyring::Entry::new("moagan", alias)
                    .get_password().map(SecretString::new)?,
            };
        }
        Ok(())
    }
}
```

(Inspirado en T09-03; T20-06 §6.3.)

---

### D.36. §38 Risks (adiciones)

| Riesgo | Mitigación | Fuente |
|---|---|---|
| Provider rolling-version changes cache | `provider_version` en cache key | T11-02 |
| Multi-tenancy en SQLite | `?mode=ro` para dashboard, single-writer para run | T16-03 |
| Disk exhaustion durante run | Pre-flight con `validate_cardinality` antes de discovery | T19-09 D15; T18-04 |
| Run zombie (kill -9 sin cleanup) | Heartbeat + recovery on startup | T20-05; T08-03 |
| Prompt injection | Detector + `--allow-injection` opt-in + 256 KiB cap | T20-10; T17-10 |
| Cache poisoning cross-run | `provider_version` en key, signature check | T11-02; T01-10 |
| Sandbox escape | `setrlimit` + denylist + `unshare` opt-in | T18-06; T07-05 |
| Crítico de plan aborta discovery | Pre-flight con `cardinality * avg_tokens <= max_storage` | T19-09 D15 |

---

### D.37. §39 Constraints (adiciones)

| Restricción existente | Adición | Fuente |
|---|---|---|
| No usar SDKs de Anthropic | Confirmar y mantener; usar `reqwest` | (confirma) |
| Rust estable 1.97.1, edition 2024 | Mantener | (confirma) |
| `async_trait` permitido | Aceptar también native AFIT opcional | T11-02 |
| Sin assets externos | `HashingEmbedder` sin deps | T18-09; T09-08; T03-07; T06-04 |
| `.env.example` versionado | Mantener | (confirma) |

---

### D.38. §40 Priorities (refinamientos)

El orden de T01-06 §40 se mantiene. Las siguientes adiciones aclaran pasos adicionales:

1. `src/secret.rs` (SecretString) — antes del primer provider.
2. `src/atomic/` (writer + journal) — antes que fases que escriben.
3. `src/llm/wire/` (Anthropic-compat, OpenAI-compat) — refactoriza §26.
4. `src/llm/circuit_breaker.rs` y `src/llm/rate_limit.rs` — junto con provider.
5. `src/telemetry/heartbeat.rs` y `src/telemetry/recover.rs` — junto con telemetría base.
6. `src/cli/doctor.rs` y `src/cli/diff.rs` y `src/cli/repair.rs` — al final, después de run/continue.

(Inspirado en T01-06 §40 + T10-07 §10; T08-03 §15.3; T11-08 §7.4.)

---

## E. Resumen ejecutivo de las 278 adiciones

| Tipo | Cantidad | % | Notas |
|---|---:|---:|---|
| Tipos nuevos (struct/enum/trait) | 67 | 24% | Enums dominan (ErrorCode, LlmError, etc.) |
| Funciones/métodos nuevos | 58 | 21% | Constructores y helpers |
| Tablas SQLite nuevas | 14 | 5% | Outbox, redact_audit, locks, etc. |
| Columnas SQLite adicionales | 19 | 7% | En runs, calls, provider_changes |
| Dependencias nuevas | 18 | 6% | Con criterios de opt-in |
| Submódulos nuevos | 14 | 5% | atomic/, wire/, embed/, etc. |
| Subcomandos CLI | 4 | 1% | doctor, diff, repair, validate |
| Flags CLI | 23 | 8% | --hash-algo, --allow-injection, etc. |
| Patrones regex de redact | 12 | 4% | anthropic, gemini, JWT, PII, etc. |
| Decisiones en §0.5 (filas 21–43) | 23 | 8% | Atomic write, BLAKE3, circuit breaker, etc. |
| Códigos de error | 65 | 23% | Stable SCREAMING_SNAKE |
| Códigos de salida | 17 | 6% | Por condición específica |
| Roles LLM nuevos | 9 | 3% | tiefighter, persona_picker, etc. |
| Constantes y magic numbers | 8 | 3% | DEFAULT_* en discovery |
| **Total** | **278** | **100%** | |

---

## F. Mapa de "qué se podría sobrescribir" vs "qué se debería añadir"

La mayoría de las 278 adiciones son **aditivas** (no tocan T01-06). Las pocas que sugieren **modificar una sección existente** de T01-06:

| Sección T01-06 | Modificación | Tipo | Justificación |
|---|---|---|---|
| §0.5 #1 (max_tokens) | No tocar (techo uniforme) | OK | T01-06 define el ceiling compartido por rol (v0.6) |
| §3.2 hash_input | Sustituir SHA-256 con BLAKE3 (manteniendo SHA-256 para export) | Sustitución menor | T02-02 §0.1; T13-04 §1.1(1) muestran que BLAKE3 es 5–10x más rápido en hot path |
| §3.3 cache | Añadir `cache_key` con quantized temperature | Aditiva | Mejora ~20% hit rate sin perder calidad |
| §4.7 retries | Añadir jitter ±50% | Aditiva | T20-02 §5.5 |
| §4.6 truncación | Focused continuation con last 500 tokens, max 2 | Sustitución menor | T20-07; T00-09 |
| §4.7 retries | Honor `Retry-After` header | Aditiva | T20-06 §6.4 |
| §5.2 patterns | Añadir 12 patrones | Aditiva | T16-06 §5.5 |
| §15.4 hibernation | Añadir CircuitBreaker y RateLimiter | Aditiva | T08-03 §5.8; T00-08 |
| §17 cardinalidad | Tabla concreta por modo (en lugar de "típica") | Sustitución menor | T15-02 §7; T18-09 |
| §25 error codes | 65 codes en lugar de 8 | Aditiva | T15-01 §0.4 |
| §34 discovery | Pre-flight con tiktoken | Aditiva | T19-09 D15 |
| §35 API key | SecretString con zeroize | Aditiva | T00-05 §13.2; T15-01 §5.5 |
| §36 (no existe) | Sección §21 explicit `paused` heartbeat + recovery | Nueva sección | T20-05 |

La regla seguida: si la modificación es **menor** (cambia una constante, un enum variant, un campo opcional), se propone como parche aditivo. Si cambia la **semántica central** (p.ej. SHA256 → BLAKE3), se documenta en este catálogo y se deja a decisión del implementador si lo aplica.

---

## G. Notas finales

- **Cero cambios incompatibles**: todas las adiciones se diseñan para no romper los 20 decisiones de T01-06 §0.5 ni el orden de §40.
- **Type-safety primero**: las adiciones usan `enum` y newtypes en lugar de strings; muchos "códigos" libres en T01-06 se elevan a tipos.
- **Reproducibilidad mejorada**: `provider_version`, `prompt_set_hash`, `config_hash`, `shared_brief_hash`, `duplicates_with` añaden garantías de re-run.
- **Observabilidad reforzada**: heartbeat, recovery on startup, `TelemetryEvent` exhaustivo, CSV summary, dashboard.html offline.
- **Seguridad por capas**: redact, SecretString, sandbox (cgroup, setrlimit, seccomp opt-in), denylist, watchdog, circuit breaker, rate limiter, prompt-injection detector.
- **Operabilidad**: 4 nuevos subcomandos (doctor, diff, repair, validate), 23 flags nuevos, exit codes estructurados.

El siguiente paso recomendado es **priorizar** las adiciones y aplicar solo las que aporten valor inmediato al MVP. La priorización natural (inspirada en T10-07 §10 + T08-03 §15.3):

1. **Día 1**: `SecretString` (#D.1.7), `AtomicWriter` (#D.1.1), `HARD_INCOMPATIBILITIES` (#D.13.15), `BLAKE3` para hashes internos (#D.6.1).
2. **Semana 1**: `SecretString`, `circuit_breaker`, `rate_limiter`, `WireFormat`, `prompt_set_hash`, `tiktoken-rs`.
3. **Mes 1**: `moagan doctor`, `moagan diff`, `moagan repair`, `moagan validate`, dashboard.html estático, heartbeat + recovery.
4. **Mes 2+**: cgroup v2, seccomp opt-in, focused continuation, streaming responses, `Embedder` trait con `HashingEmbedder`.

---

## Decision table v0.3 patch

| Patch | Decision |
|---|---|
| J | `Pipeline::resume(last_phase)` skips phases up to and including `last_phase`; canonical order comes from `phase_index()`. `rerun` runs the full pipeline from `intake`. |
| K | `HARD_INCOMPATIBILITIES` is enforced at `SynthesizePhase` only, not at `ProposePhase`, because the proposal catalogue is too coarse for tag-level enforcement. |
| L | `ReportingLayer` for tracing is best-effort; the inner layer is the source of truth and the dispatcher is invoked in `on_event` after the inner layer forwards. |
| M | `ErrorCode` is additive on top of `Error`; existing `Error` variants remain and `code()` maps them to a stable public-facing code. |
| N | `strip_secrets` is applied to args before spawn; stdin redaction is a follow-up. |
| O | `Rubric` is a reference table; the LLM is not forced to use it and `evaluate_with_rubric` remains opt-in. |
| P | Three new roles are added to the registry but not wired into any phase; they are opt-in for callers. |
