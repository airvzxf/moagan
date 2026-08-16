# Prompt de handoff — `moagan` v0.8 work session (cierre de sesión 4)

> **Propósito**: arrancar una sesión nueva de opencode en
> `/home/wolf/workspace/projects/moagan` con el máximo contexto
> posible al cierre formal de la sesión 4 (v0.8.0 released).
> Léeme entero antes de hacer cualquier otra cosa.

---

## 1. TL;DR

HEAD `4dcd740 feat(research): per-host bearer token resolution
(K.4 sub-3) (#498)`. Local in sync con `origin/main`. Tag `v0.8.0`
anotado y pusheado (post-release-bump merge). Crate version
`0.8.0` (Cargo.toml). Test suite verde: `cargo test --all-targets`
reporta `1817 + 33 = 1850 passed; 0 failed`. **11 PRs** squash-
mergeados en sesión 4 desde el último handoff formal
(`docs/handoff-next-session.md` cubre hasta v0.4 / HEAD `693d746`;
los cierres v0.5-v0.7.2 están en `docs/v0.5-final-report.md` /
`docs/v0.6-final-report.md` / `docs/v0.7-final-report.md`). Tracks
cerrados: **decisiones del operador (D-1, D-2, D-3, D-4)**,
**Tier A residuals (A#8/#9/#10, A#11, A#12, A#13, A#14, A#15)**,
**Tier B opt-ins (B#18, B#20 sub-3)**. PRs #479 (ADR-0001) +
#480 (PR #421 closure) + #482 (audit_e2e flake diag) + #484 (K.4
advanced) ship la capa de decisiones; #486 + #488 + #490 + #492 +
#494 ship los Tier A residuals; #496 + #498 ship los Tier B
opt-ins. ADR-0001 formaliza la política diferenciada de la no-go
list (`petgraph` y `proptest` admitidos, `comfy-table` deferido).
`docs/v0.8-final-report.md` documenta el catálogo completo.

## 2. Tabla de los 11 PRs mergeados (Sesión 4)

(Orden cronológico inverso — newest first. Cada PR cierra uno o
más items del catálogo D.x, del Tier A residuals, del Tier B
opt-ins, o de las decisiones del operador D-1..D-4.)

### Cluster A — Decisiones del operador (4 PRs)

| # | PR | D.x / categoría | Capacidad |
|---:|---:|---|---|
| 1 | #484 | D-4 / Tier A #15 | `Retry-After` parsing + jittered backoff + per-host circuit breaker (K.4 rate-limiting advanced) |
| 2 | #482 | D-3 / decisión | `docs/audit-e2e-flake-diagnose-2026-08-16.md` — formaliza `audit_e2e` flake como opt-in permanente |
| 3 | #479 | D-2 / ADR | `docs/adr/0001-no-go-list-policy.md` + `scripts/check-no-forbidden-crates.sh` — política diferenciada (`petgraph` admit, `proptest` dev-only, `comfy-table` defer) |
| 4 | #480 | D-1 / docs | `docs/pr421-closure-rationale.md` — absorption matrix de los 10 PRs que absorbieron el contenido de #421 |

### Cluster B — Tier A residuals (5 PRs)

| # | PR | D.x / categoría | Capacidad |
|---:|---:|---|---|
| 5 | #486 | D.13.x / A#11 | `Role::ContradictionJudge` reemplaza el stub ligero de contradicciones; prompt `src/prompts/contradiction_judge_v1.md` |
| 6 | #488 | D.22 / A#12 | 5 patterns adicionales: `shared_blind_spots`, `unanimous_claims_without_evidence`, `hidden_assumptions`, `omitted_risks`, `unverified_claims` |
| 7 | #490 | A#8/#9/#10 | matrix jobs `discover_opencode_go` + `discover_deepseek` con `--ignored` step en `.github/workflows/e2e-network.yml` |
| 8 | #492 | D.29-D.32 / A#14 | `moagan telemetry cleanup --cross-run` — sweep de filas SQLite huérfanas |
| 9 | #494 | D.23-D.27 / A#13 | `SaturationEvent` runtime + `moagan telemetry alerts list` + integración `tokio::sync::watch` para push in-process |

### Cluster C — Tier B opt-ins (2 PRs)

| # | PR | D.x / categoría | Capacidad |
|---:|---:|---|---|
| 10 | #496 | B#18 | `RemoteEmbedder` adapter HTTP con 4 wire formats (OpenAI/Cohere/Voyage/Custom) |
| 11 | #498 | B#20 sub-3 / K.4 | `HostPolicy` extendido con bearer tokens por host desde `[research.auth]` config |

## 3. Deferred items restantes (no atacados)

Ordenados por valor/costo descendente. El estado refleja HEAD
`4dcd740` y tag `v0.8.0` (2026-08-16).

### 3.1 K.4 sub-1 — PDF parser

- PDFs (B#20 sub-1) NO atacados en sesión 4.
- Trade-off: `lopdf` crate vs shelling out a `pdftotext`. La
  ruta `lopdf` requiere justificación re: no-go list (no entra
  en release binary). La ruta `pdftotext` shelling es la
  canónica.
- Coste: 3–5 d.

### 3.2 K.4 sub-2 — JS rendering

- JS rendering (B#20 sub-2) NO atacados. Diferido a v0.9+ por
  coste (Chromium / Playwright > 100 MB).
- Sin ticket.

### 3.3 GLM / Qwen / Kimi providers

- 3/3 providers sin código de producción, sólo entries de
  catálogo + asserts en `verify-spec-gaps §1.3`.
- Bloqueados por ticket del operador (decisión de diseño, no
  olvido).

### 3.4 Cross-platform sandbox fallback (macOS / WSL)

- Sandbox hardening (Tracks E + D.11.x) es Linux-only.
  `unshare(CLONE_NEW*)` + cgroup v2 + seccomp BPF no tienen
  equivalente en macOS / WSL.
- Trade-off importante: spec §11.4 asume Linux; v0.9+ queda
  pendiente.

### 3.5 `process_locks` lease module (D.1.5)

- Schema `process_locks` creada (v008). Las funciones
  `acquire_process_lock` / `release_process_lock` siguen
  scaffolding hasta que exista un `src/storage/lease.rs`
  module con fence monótono (`holder` keyed con FencingToken) +
  heartbeats.
- Diferido por falta de demanda concreta.

### 3.6 Dashboard cross-run analytics

- J3 (`/api/lineage` cross-run endpoint) shippeado. Quedan
  otras vistas cross-run (`/api/compare-runs`,
  `/api/aggregates`) como opt-in del catálogo.

### 3.7 `comfy-table` para tablas CLI

- ADR-0001 difiere `comfy-table` a v0.9. Texto plano funciona y
  los tests visuales con `insta` cubren regresiones de tabla.

### 3.8 `petgraph` DAG backend (opcional)

- ADR-0001 admite `petgraph 0.6` con `optional = true` bajo
  `feature = "dag"`. El default build sigue siendo el vector
  lineal `phases/`. Implementación pendiente (v0.9).

### 3.9 `proptest` para property-based testing

- ADR-0001 admite `proptest 1.4` en `[dev-dependencies]` sólo.
  Ningún property test añadido todavía (v0.9).

### 3.10 HardIncompat extensions beyond catálogo I.6

- PR #216 (sesión E) cerró la lista exhaustiva de 6 variantes.
  Variantes adicionales (`ClusterLocalInGlobal`,
  `PullInPushOnly`, `StatelessInStateful`) son opt-in para una
  sub-fase futura si surge demanda.

### 3.11 Track C path B wire-up (research-json)

- Track C (#202, E8 wire) shippeado. La path B (LLM-side
  re-call con schema completo) queda deferred por decisión
  histórica del usuario (no arreglar JSON output forzado, sólo
  hacerlo tolerante — path A + B + C shipped en #117, #119,
  #201).

### 3.12 Multimodal streaming

- Bloqueado upstream. Sin acción.

## 4. Convenciones

### 4.1 Commit signing (mandatory)

- GPG key global: `414687A3CD7E65B9` (program `gpg2`).
- `commit.gpgsign = true` en `~/.gitconfig` (global, heredado
  por este repo).
- Conventional Commits en inglés: `feat`, `fix`, `refactor`,
  `docs`, `test`, `chore`, `ci`, `build`, `perf`. Scope
  opcional (`feat(cli):`, `fix(build):`). Subject ≤ 72 chars,
  body explica el *por qué* (no el *qué*).
- **Nunca** `--no-gpg-sign`, `--amend`, `--force` (incluyendo
  `--force-with-lease`).
- Pre-push verification:
  `git log --pretty="%H %G? %s" origin/<branch>..HEAD` — todas
  las líneas deben empezar con `G`.

### 4.2 No-go list (actualizada con ADR-0001, PR #479)

- **NO** Anthropic SDK crates (`anthropic-*`, `claude-*`).
  `scripts/check-no-anthropic-sdk.sh` lo enforce.
- **NO** `secrecy` crate. Usar `moagan::secret::SecretString`
  con `zeroize`.
- **NO** `axum`, `hyper`, `sqlx`, `governor`, `figment`,
  `refinery`, `askama`, `handlebars`, `lettre`, `inquire`,
  `time` crate.
- **NO** mutable globals, no `lazy_static`, no
  `Mutex<Option<T>>` para state.
- **NO** `tokio::spawn` sin un `JoinHandle` grabado o un
  `CancellationToken` parent.
- **NO** secret literals en código, CLI flags, o config files
  committed.

**Differentiated allow-list (per ADR-0001)** — supersedes the
blanket prohibition for these three crates:

- **`petgraph 0.6`** — admitted `[dependencies]` con
  `optional = true` (Cargo feature `dag`). Default build no lo
  pulla; `phases/` vector lineal sigue siendo el path por
  defecto.
- **`comfy-table 7.1`** — DEFERRED a v0.9. Texto plano funciona
  y los tests visuales con `insta` cubren regresiones de tabla.
- **`proptest 1.4`** — admitted `[dev-dependencies]` sólo. Un
  `[dependencies]` row para `proptest` es rechazado. El crate
  no entra al release binary.

`scripts/check-no-forbidden-crates.sh` enforza las tres
políticas. Corre en `make lint` (Tier T1).

### 4.3 Validation gauntlet (los 4 gates)

```bash
make fmt-check                        # formatter diff = empty
make guard-deps                       # no anthropic SDK + forbidden crates guard
make lint                             # cargo clippy --all-targets -- -D warnings
make build                            # cargo build
make test-ci                          # cargo test, skips known-flaky audit_e2e
make smoke                            # fast mode with mock + minimax
make e2e                              # integration suite
make e2e-network                      # main branch only (real LLM providers)
```

T0/T1/T2/T3 tiers documentados en
[`docs/validation-tiers.md`](validation-tiers.md).

### 4.4 Smoke gates

```bash
moagan run --mode fast --provider mock
# → debe producir final/portfolio.md + rankings/ranking.json

MINIMAX_API_KEY=<key> moagan run --mode fast --provider minimax
# → debe producir los mismos artefactos + telemetry/calls.jsonl.gz
```

## 5. PR protocol 10-step

Repo OWNER (`airvzxf/moagan`, permission ADMIN). Squash-merge
habilitado. Copilot review habilitado pero a menudo
quota-exhausted.

```bash
# 1. Branch off main
git checkout main && git pull --ff-only
git checkout -b <type>/<scope>          # feat/, fix/, refactor/, docs/, test/, chore/, ci/, build/, perf/

# 2. Cambios + gauntlet local
make fmt-check && make guard-deps
make lint && make build
make test-ci

# 3. Commit firmado GPG
git add -A && git commit -m "..."
git log --pretty="%H %G? %s" HEAD~1..HEAD   # debe ser G

# 4. Push + verify
git push -u origin <branch>
git log --pretty="%H %G? %s" origin/main..HEAD   # todas G

# 5. Issue
gh issue create --title "..." --label "..." --body "..."

# 6. PR con Closes #N
gh pr create --base main --head <branch> --title "..." --body "..."

# 7. CI + Copilot review
~/.config/opencode/scripts/gh-pr-wait.sh <N>

# 8. Apply feedback / resolve threads
gh api /repos/<owner>/<repo>/pulls/<N>/comments | jq -c '.[] | {id, path, body}'
gh-pr-resolve-thread.sh <N> <comment-id> "<reply-body>"

# 9. Squash-merge OWNER
gh pr merge <N> --squash --delete-branch --admin

# 10. Verify
gh issue view <issue> --json state    # CLOSED
git fetch origin main && git log --oneline -3
```

El squash-merge commit en `main` aparece firmado por la
web-flow key de GitHub (`C` en `git log --pretty="%G?"`). Es
esperado.

## 6. Plan para la siguiente sesión (sesión 5 / v0.9 prospect)

Status al cierre de HEAD `4dcd740` + tag `v0.8.0` (2026-08-16):

- ✅ **4 decisiones del operador (D-1, D-2, D-3, D-4)** —
  cerradas con PR #480 (closure rationale), #479 (ADR-0001),
  #482 (audit_e2e flake diag), #484 (K.4 advanced).
- ✅ **Tier A residuals (5 items)** — cerrados con PR #486
  (A#11), #488 (A#12), #490 (A#8/#9/#10), #492 (A#14), #494
  (A#13).
- ✅ **Tier B críticos (2 items)** — cerrados con PR #496
  (B#18 RemoteEmbedder), #498 (B#20 sub-3 K.4 multi-host
  auth).

Tracks abiertos al cierre (ordenados por valor/costo):

### 6.1 K.4 sub-1 — PDF parser

- L-sized (1 PR). B#20 sub-1.
- Wiring: `src/research/fetcher.rs` con PDF content-type
  handler (vía `lopdf` o `pdftotext` shelling).
- Trade-off: `lopdf` requiere justificación re: no-go list;
  `pdftotext` shelling es la ruta canónica.

### 6.2 `proptest` adoption

- S-sized (1 PR). ADR-0001 lo admite en `[dev-dependencies]`.
- Targets naturales: cache hash invariants, sha256 of context,
  ID generation invariants.

### 6.3 `petgraph` DAG backend (opcional)

- M-sized (1 PR). ADR-0001 lo admite con `optional = true`
  bajo `feature = "dag"`. Default build no lo pulla.
- Wiring: `src/phases/dag.rs` con `DagNode` trait esbozado en
  T01-06 §3.6.2, §3.5.2.

### 6.4 GLM / Qwen / Kimi providers

- XL-sized (3 PRs, 9–15 d total). Bloqueados por ticket.
- Si se aprueba: `src/llm/glm.rs` (ChatML / OpenAI-compatible),
  `src/llm/qwen.rs` (ChatML / DashScope), `src/llm/kimi.rs`
  (Moonshot API). Wire-up en `provider_pool.rs` +
  `capability.rs` con `max_token_auto`.

### 6.5 Cross-platform sandbox fallback (macOS / WSL)

- XL-sized (1+ PR). Trade-off: `unshare(CLONE_NEW*)` + cgroup
  v2 + seccomp BPF son Linux-only.

### 6.6 `process_locks` lease module (D.1.5)

- M-sized (1 PR). Schema `v008` ya existe; queda implementar
  `src/storage/lease.rs` con FencingToken + heartbeats.

### 6.7 Deferred a v0.9+

- K.4 PDF parser (necesita decisión de implementación).
- Cross-platform sandbox fallback (trade-off importante).
- `petgraph` DAG (opcional; sin demanda explícita).
- `comfy-table` para CLI (deferido per ADR-0001).

---

## Anexo A — Topología del código (recordatorio)

```
src/
├── lib.rs                       # entry público, dispatch al CLI
├── main.rs                      # entry binario, dotenvy::dotenv() auto-load
├── atomic/                      # AtomicWriter
├── audit/                       # audit proxy + verify (sidecar HTTP recorder)
├── cancel.rs                    # Cancel + CancelTier (3-tier)
├── checkpoint/                  # checkpoint humano (stdin-driven)
├── cli/                         # 19 sub-comandos top-level + AuditCmd + TelemetryCmd
├── config.rs                    # Config (server, retention, stability, ...)
├── context/                     # ContextRef + Loader
├── discovery/                   # matrix, tagger, clusterer, facet,
│                                # facet_cache, extractor, integrator,
│                                # contradiction (LLM-as-judge, #486),
│                                # epistemic_legacy, pause, coordinator
├── domain/                      # Brief, Proposal, Sketch, Judge, Ranking,
│                                # Manifest, FinalReport, AdversaryReport, ...
├── error.rs                     # Error + ExitCode mapping
├── error_code.rs                # ErrorCode enum (60+ SCREAMING_SNAKE_CASE)
├── execution/                   # Parallelism (semáforo)
├── fs_layout.rs                 # RunPaths::resolve() + MoaganHome
├── ids.rs                       # RunId UUID v7
├── llm/                         # Providers + CircuitBreaker + RateLimiter +
│                                # RetryBudget + Cache + Wire + Embed +
│                                # embedder_remote (RemoteEmbedder, #496) +
│                                # 28 roles (Role::ContradictionJudge added, #486)
├── modal_gate.rs
├── phases/                      # 28 archivos, fase por fase del pipeline
├── preferences/
├── ranking/                     # pareto + diversity + cluster + rubric + stability
├── reconcile/
├── redact/                      # RedactPolicy + patterns + writer
├── research/                    # allowlist + fetcher (K.4 advanced, #484) +
│                                # auth (#498) + host_policy + circuit_breaker
├── sandbox/                     # Sandbox subprocess + allowlist + denylist +
│                                # strip_secrets + verify_binary_exists + BPF
├── secret.rs                    # SecretString con zeroize
├── storage/                     # SQLite + migrations/ + compression + leases
├── telemetry/                   # dashboard + export + verify + retention +
│                                # alerts + saturation (#494)
├── test_support.rs
├── time.rs
└── validators/                  # structural + rust + python + typescript + sql +
                                 # schema + constraints
```

## Anexo B — Documentos normativos (orden de lectura)

1. `docs/proposal-01-concept.md` — visión conceptual (V4).
2. `docs/proposal-02-rust.md` — spec técnica (T01-06).
3. `docs/proposal-03-add-ons.md` — catálogo aditivo (D.x),
   con v0.8.0 markers (D.22 DONE, D.23-D.27 DONE, D.29-D.32
   DONE, K.4 sub-3 DONE).
4. `docs/proposal-04-cuarta-etapa.md` — cuarta etapa
   (K.1-K.4 shippeados, K.4 sub-1 PDF deferred, K.4 sub-2 JS
   render deferred).
5. `docs/v0.7-final-report.md` — v0.7.1 / v0.7.2 final report.
6. `docs/v0.8-final-report.md` — v0.8.0 final report (este
   release).
7. `docs/pending-items-2026-08-13.md` — informe de pendientes
   base para sesión 4 (cerrado).
8. `docs/handoff-next-session.md` — handoff histórico (v0.4;
   ahora superseded por este documento).
9. `docs/handoff-session-4-2026-08-16.md` — este archivo.
10. `docs/adr/0001-no-go-list-policy.md` — ADR-0001
    (differentiated no-go list policy).
11. `docs/inconsistencies-audit-2026-08-12*.md` — auditorías
    de inconsistencias (rounds 1-12).
12. `AGENTS.md` — convenciones + no-go list actualizada con
    las entradas allow-list diferenciadas de ADR-0001.

## Anexo C — Entorno local

- **OS**: Arch Linux rolling release, kernel 7.1.3-arch2-1.
- **Shell**: bash 5.x + tmux 3.x.
- **Pinentry**: GUI (`pinentry-gtk`); `default-cache-ttl
  31536000` (1 año).
- **SSH**: `github_airvzxf_ed25519` cargado en ssh-agent
  (GitHub).
- **No CGO**: `rusqlite` se compila bundled (`bundled`
  feature).
- **Test runtime**: `cargo test --lib` ~32s; integration ~50s.

### Helpers útiles

```bash
# Verificar firma de un commit
git log --show-signature -1 <SHA>

# Contar tests
cargo test --lib 2>&1 | grep "test result:"

# Diff entre origin/main y HEAD
git diff origin/main..HEAD --stat

# Squash-merge CI status
gh pr view <N> --json statusCheckRollup

# Issue tracking
gh issue list --label "enhancement" --state open

# Verificar no-go list guard
make guard-deps
./scripts/check-no-forbidden-crates.sh

# Tag verification
git tag --list 'v0.*' | tail -5
git ls-remote origin refs/tags/v0.8.0
```

---

Signed-off-by: opencode (sesión 4 closure)