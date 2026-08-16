# Prompt de handoff — `moagan` v0.9 work session (cierre de sesión 5)

> **Propósito**: arrancar una sesión nueva de opencode en
> `/home/wolf/workspace/projects/moagan` con el máximo contexto
> posible al cierre formal de la sesión 5 (v0.9.0 released).
> Léeme entero antes de hacer cualquier otra cosa.

---

## 1. TL;DR

HEAD `ebaaa3d feat(phases): optional petgraph DAG backend
(feature = 'dag') (#520)`. Local in sync con `origin/main`. Tag
`v0.9.0` anotado y pusheado (post-release-bump merge). Crate
version `0.9.0` (Cargo.toml). Test suite verde:
`cargo test --all-targets` reporta `1951 + 41 = 1992 passed;
0 failed`. **`cargo test --features dag --all-targets` reporta
`1958 + 41 = 1999 passed; 0 failed`**. **9 PRs substantivos**
squash-mergeados en sesión 5 desde el último handoff formal
(`docs/handoff-session-4-2026-08-16.md` cubría hasta v0.8.0 /
HEAD `4dcd740`; los cierres v0.4-v0.8.0 están en
`docs/v0.4-final-report.md` / `docs/v0.5-final-report.md` /
`docs/v0.6-final-report.md` / `docs/v0.7-final-report.md` /
`docs/v0.8-final-report.md`). Tracks cerrados: **ADR-0001
allow-list adoption** (3 crates: `petgraph`, `proptest`,
`comfy-table` deferido), **deferred v0.9 prospect backlog**
formalizado en `docs/deferred-v0-9-2026-08-16.md` (#502), y los
**8 items que SÍ se hacen en v0.9** (SaturationSink wiring #508,
ProcessLease API #506, HardIncompat extensions #504, AsyncEmbedder
trait #512, proptest adoption #510 + #514 hotfix, dashboard
cross-run analytics #516, K.4 PDF parser #518, petgraph DAG
backend #520). `docs/v0.9-final-report.md` documenta el catálogo
completo.

---

## 2. Tabla de los 9 PRs substantivos mergeados (Sesión 5)

(Orden cronológico inverso — newest first. Cada PR cierra uno o
más items del catálogo D.x, del deferred v0.9 prospect, o de las
allow-list entries de ADR-0001.)

| # | PR | D.x / categoría | Capacidad |
|---:|---:|---|---|
| 1 | #520 | T01-06 §3.6.2 / ADR-0001 D-1 | `petgraph 0.6 + serde` como Cargo feature `dag` (default-off); `src/phases/dag.rs` con Kahn topological layers + `execute_dag` (`futures::join_all`) |
| 2 | #518 | B#20 sub-1 / K.4 | PDF parsing vía `pdftotext` shelling out (`src/research/pdf.rs`); 0 nuevas deps, cumple ADR-0001; requiere `poppler-utils` |
| 3 | #516 | T01-06 §14.4 / D.29-D.32 | `GET /api/compare-runs` + `GET /api/aggregates`; 0 nuevas deps |
| 4 | #514 | Test fix-up de #510 | 1-line `prop_assume!(model != other_model)` en `src/llm/cache/mod.rs:931` |
| 5 | #510 | ADR-0001 D-3 | `proptest = "1.4"` exclusivo en `[dev-dependencies]`; 30 nuevos property tests en 5 archivos (`ids.rs`, `llm/cache`, `llm/wire.rs`, `rate_limiter.rs`, `compression.rs`) |
| 6 | #512 | B#18 follow-up | `AsyncEmbedder` trait + `block_in_place` sync bridge para `RemoteEmbedder`; el clusterer queda sync |
| 7 | #506 | D.1.5 (deferred) | `ProcessLease` API tipada con `FencingToken` monótono + `heartbeat_process_lock` + schema `v019` |
| 8 | #508 | D.23 + D.27 (#494 follow-up) | `SaturationSink` wireado en `BreakeredProvider` al registry time; `with_saturation_sink` / `attach_saturation_sink` / `registry_from_config_with_sink` / `with_home_and_sink` |
| 9 | #504 | D.13.15 (opt-in catalog) | 3 nuevos variants en `HardIncompat`: `ClusterLocalInGlobal`, `PullInPushOnly`, `StatelessInStateful` |

**Plus 1 meta-documentation PR** (no cuenta hacia el feature batch):

| PR | Título | Capacidad |
|---|---|---|
| #502 | docs(v0-9): formalize deferred items | `docs/deferred-v0-9-2026-08-16.md` (153 líneas) — formaliza NO HACER (GLM/Qwen/Kimi + cross-platform sandbox) + lista items que SÍ se hacen con coste y política de aborto |

---

## 3. Deferred items restantes (no atacados)

Ordenados por valor/costo descendente. El estado refleja HEAD
`ebaaa3d` y tag `v0.9.0` (2026-08-16).

### 3.1 CLI wire-up de `maybe_run_via_dag` cuando `mode == "deep"`

- El dispatcher está en `src/phases/pipe.rs` (#520, gated,
  +37 LoC). Sólo falta llamar desde
  `cli/run.rs::build_pipeline_for_mode` (~5 líneas).
- Coste: < 1 PR (S-sized).

### 3.2 K.4 wire-up de `fetcher::fetch_one` para PDFs

- El parser `src/research/pdf.rs::fetch_pdf_text` está shippeado
  (#518). Falta el dispatch en `src/research/fetcher.rs` para
  rutear PDFs al nuevo parser (mirar `Content-Type: application/pdf`
  o URL con extensión `.pdf`).
- Coste: 1 PR (S-sized).

### 3.3 HardIncompat detector wiring para los 3 nuevos variants

- Las typed records de `ClusterLocalInGlobal`, `PullInPushOnly`,
  `StatelessInStateful` están en `src/domain/constraint.rs` (#504).
  Falta el detector propiamente (la heurística que dispara cada
  variant); ahora mismo son stubs que no disparan.
- Coste: 1 PR (M-sized).

### 3.4 `process_locks` heartbeat sweep / GC

- El lease module `src/storage/lease.rs` está shippeado (#506)
  con `acquire_process_lock` / `heartbeat_process_lock` /
  `release_process_lock`. Falta un sweepper periódico que
  libere leases con `expires_at_unix` vencido.
- Coste: 1 PR (M-sized).

### 3.5 K.4 sub-2 — JS rendering

- JS rendering (B#20 sub-2) NO atacado. Diferido a v0.10+ por
  coste (Chromium / Playwright > 100 MB).
- Sin ticket.

### 3.6 GLM / Qwen / Kimi providers

- 3/3 providers sin código de producción, sólo entries de
  catálogo + asserts en `verify-spec-gaps §1.3`.
- Bloqueados por ticket del operador (decisión de diseño
  formalizada en
  `docs/deferred-v0-9-2026-08-16.md §1.1`, no atacar hasta
  que el operador tenga API keys).

### 3.7 Cross-platform sandbox fallback (macOS / WSL)

- Sandbox hardening (Tracks E + D.11.x) es Linux-only.
  `unshare(CLONE_NEW*)` + cgroup v2 + seccomp BPF no tienen
  equivalente en macOS / WSL.
- Decisión del operador formalizada en
  `docs/deferred-v0-9-2026-08-16.md §1.2`: único público
  objetivo = Linux puros.

### 3.8 `comfy-table` para tablas CLI

- ADR-0001 difiere `comfy-table` a v0.9. **v0.9.0 ya cerró
  sin adoptarlo** — sigue diferido por falta de demanda
  concreta. Re-evaluar si surge use-case.

### 3.9 Multimodal streaming

- Bloqueado upstream. Sin acción.

---

## 4. Convenciones

### 4.1 Commit signing (mandatory)

- GPG key global: `414687A3CD7E65B9` (program `gpg2`).
- `commit.gpgsign = true` en `~/.gitconfig` (global, heredado
  por este repo).
- Conventional Commits en inglés: `feat`, `fix`, `refactor`,
  `docs`, `test`, `chore`, `ci`, `build`, `perf`. Scope
  opcional (`feat(cli):`, `fix(build):`). Subject ≤ 72 chars,
  body explica el *por qué* (no el *qué*).
- Scope regex `[a-z0-9_-]+`, **NO** dots — usa `v0-9` en vez
  de `v0.9` (conventional-commits standard).
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
  defecto. **Adoptado en v0.9 (#520)** con `topological_layers`
  (Kahn) + `execute_dag` (`futures::join_all`).
- **`comfy-table 7.1`** — DEFERRED. Sin adopt en v0.9. Texto
  plano funciona y los tests visuales con `insta` cubren
  regresiones de tabla.
- **`proptest 1.4`** — admitted `[dev-dependencies]` sólo. Un
  `[dependencies]` row para `proptest` es rechazado. El crate
  no entra al release binary. **Adoptado en v0.9 (#510)** con
  30 property tests en 5 archivos hash-heavy.

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

### 4.5 K.4 PDF fetcher smoke (nuevo en v0.9)

El nuevo PDF parser (#518) requiere `poppler-utils` instalado:

```bash
pacman -S poppler          # Arch
apt install poppler-utils  # Debian/Ubuntu
```

Sin `poppler-utils`, el fetcher retorna
`Error::ResearchUnavailable` con hint de install — no falla en
silencio.

---

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

---

## 6. Plan para la siguiente sesión (sesión 6 / v0.10 prospect)

Status al cierre de HEAD `ebaaa3d` + tag `v0.9.0` (2026-08-16):

- ✅ **ADR-0001 allow-list adoption** — `petgraph` (gated
  `feature = "dag"`, #520) + `proptest` (dev-only, #510) +
  `comfy-table` deferred.
- ✅ **Deferred v0.9 prospect backlog** — 9 PRs substantivos
  mergeados (#502 meta-doc + 8 items de la lista "SÍ se hacen
  en v0.9" del `docs/deferred-v0-9-2026-08-16.md §2`).
- ✅ **Follow-ups de v0.8 cerrados** — #494 follow-up (#508
  SaturationSink wiring), #496 follow-up (#512 AsyncEmbedder
  bridge), K.4 sub-1 (#518 PDF parser).

Tracks abiertos al cierre (ordenados por valor/costo):

### 6.1 CLI wire-up de `maybe_run_via_dag`

- S-sized (1 PR, ~5 líneas).
- `cli/run.rs::build_pipeline_for_mode` debe llamar
  `maybe_run_via_dag` cuando `mode == "deep"` y
  `cfg!(feature = "dag")`.
- El follow-up queda explícito en el body de #520.

### 6.2 K.4 wire-up de `fetcher::fetch_one` para PDFs

- S-sized (1 PR).
- `src/research/fetcher.rs` debe rutear PDFs (Content-Type
  `application/pdf` o URL con extensión `.pdf`) al nuevo
  parser `src/research/pdf.rs::fetch_pdf_text`.

### 6.3 HardIncompat detector wiring para los 3 nuevos variants

- M-sized (1 PR).
- `ClusterLocalInGlobal`, `PullInPushOnly`,
  `StatelessInStateful` están en `src/domain/constraint.rs`
  como typed records pero sin detector. Falta la heurística.

### 6.4 `process_locks` heartbeat sweep / GC

- M-sized (1 PR).
- Sweepper periódico (background task) que libera leases con
  `expires_at_unix` vencido. Necesita `CancellationToken` parent
  per no-go list.

### 6.5 Deferred a v0.10+

- K.4 sub-2 JS rendering (cost 5+ d, sin ticket).
- GLM/Qwen/Kimi providers (bloqueados por ticket del operador).
- Cross-platform sandbox (bloqueado por decisión del operador).
- `comfy-table` para CLI (re-evaluar ADR-0001 §D-2).
- Multimodal streaming (bloqueado upstream).

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
│                                # Manifest, FinalReport, AdversaryReport,
│                                # constraint (HardIncompat + 3 opt-ins, #504)
├── error.rs                     # Error + ExitCode mapping
│                                # (+ResearchUnavailable, #518)
├── error_code.rs                # ErrorCode enum (60+ SCREAMING_SNAKE_CASE)
├── execution/                   # Parallelism (semáforo)
├── fs_layout.rs                 # RunPaths::resolve() + MoaganHome
├── ids.rs                       # RunId UUID v7
│                                # (+proptest blocks, #510)
├── llm/                         # Providers + CircuitBreaker + RateLimiter +
│                                # RetryBudget + Cache + Wire + Embed +
│                                # embedder_remote (RemoteEmbedder, #496) +
│                                # embed/ (AsyncEmbedder trait, #512) +
│                                # 28 roles (Role::ContradictionJudge added, #486)
├── modal_gate.rs
├── phases/                      # 28 archivos, fase por fase del pipeline
│                                # (+dag.rs gated, +maybe_run_via_dag gated, #520)
├── preferences/
├── ranking/                     # pareto + diversity + cluster + rubric + stability
├── reconcile/
├── redact/                      # RedactPolicy + patterns + writer
├── research/                    # allowlist + fetcher (K.4 advanced, #484) +
│                                # auth (#498) + host_policy + circuit_breaker +
│                                # pdf (pdftotext, #518)
├── sandbox/                     # Sandbox subprocess + allowlist + denylist +
│                                # strip_secrets + verify_binary_exists + BPF
├── secret.rs                    # SecretString con zeroize
├── storage/                     # SQLite + migrations/ + compression + leases
│                                # (ProcessLease + FencingToken + heartbeats,
│                                # schema v019, #506)
├── telemetry/                   # dashboard (compare-runs, aggregates, #516) +
│                                # export + verify + retention + alerts +
│                                # saturation (#494, #508 wiring)
├── test_support.rs
├── time.rs
└── validators/                  # structural + rust + python + typescript + sql +
                                 # schema + constraints
```

---

## Anexo B — Documentos normativos (orden de lectura)

1. `docs/proposal-01-concept.md` — visión conceptual (V4).
2. `docs/proposal-02-rust.md` — spec técnica (T01-06).
3. `docs/proposal-03-add-ons.md` — catálogo aditivo (D.x),
   con v0.9.0 markers (D.1.5 DONE, D.13.15 +3 variants, D.23 +
   D.27 wired, K.4 sub-1 DONE).
4. `docs/proposal-04-cuarta-etapa.md` — cuarta etapa
   (K.1-K.4 sub-1 DONE, sub-2 deferred).
5. `docs/v0.8-final-report.md` — v0.8.0 final report
   (predecessor).
6. `docs/v0.9-final-report.md` — v0.9.0 final report (este
   release).
7. `docs/deferred-v0-9-2026-08-16.md` — v0.9 NOT-DO list
   (formalizado por #502).
8. `docs/handoff-next-session.md` — handoff histórico (v0.4;
   superseded por este documento).
9. `docs/handoff-session-4-2026-08-16.md` — handoff al cierre
   de sesión 4.
10. `docs/handoff-session-5-2026-08-16.md` — este archivo.
11. `docs/adr/0001-no-go-list-policy.md` — ADR-0001
    (differentiated no-go list policy; honrado por v0.9).
12. `docs/inconsistencies-audit-2026-08-12*.md` — auditorías
    de inconsistencias (rounds 1-12).
13. `AGENTS.md` — convenciones + no-go list actualizada con
    las entradas allow-list diferenciadas de ADR-0001.

---

## Anexo C — Entorno local

- **OS**: Arch Linux rolling release, kernel 7.1.3-arch2-1.
- **Shell**: bash 5.x + tmux 3.x.
- **Pinentry**: GUI (`pinentry-gtk`); `default-cache-ttl
  31536000` (1 año).
- **SSH**: `github_airvzxf_ed25519` cargado en ssh-agent
  (GitHub).
- **No CGO**: `rusqlite` se compila bundled (`bundled`
  feature).
- **Test runtime**: `cargo test --lib` ~45s; integration ~70s.
- **poppler-utils**: requerido para K.4 PDF fetcher (#518).
  `pacman -S poppler` (Arch) / `apt install poppler-utils`
  (Debian/Ubuntu).

### Helpers útiles

```bash
# Verificar firma de un commit
git log --show-signature -1 <SHA>

# Contar tests
cargo test --lib 2>&1 | grep "test result:"

# Test con feature 'dag' habilitada
cargo test --features dag --all-targets 2>&1 | grep "test result:"

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
git tag --list 'v0.*' | tail -7
git ls-remote origin refs/tags/v0.9.0

# Verificar que pdftotext está disponible (K.4 #518)
which pdftotext
```

---

## Anexo D — Resumen de cambios v0.8.0 → v0.9.0

- **9 PRs substantivos** mergeados (`#502` meta-doc +
  `#504, #506, #508, #510, #512, #514, #516, #518, #520`
  features).
- **Cargo.toml**: `0.8.0 → 0.9.0`.
- **17 fixtures** `register_run(..., "0.8.0", ...)` → `"0.9.0"`
  en `src/storage/sqlite.rs`, `src/telemetry/dashboard.rs`,
  `tests/integration_phase_i.rs`,
  `tests/integration_telemetry_saturation.rs`.
- **Schema migration v019**: `process_locks ADD COLUMN
  last_heartbeat_unix INTEGER NOT NULL DEFAULT 0` (#506).
- **Cargo feature `dag`**: `petgraph 0.6 + serde` opcional,
  default-off (#520).
- **`proptest 1.4`** en `[dev-dependencies]`, no en release
  binary (#510).
- **`Error::ResearchUnavailable`** variant (#518).
- **30 new property tests** (#510), 1 line fix (#514).
- **2 new dashboard endpoints** (`/api/compare-runs`,
  `/api/aggregates`, #516).
- **2 new docs**: `docs/v0.9-final-report.md`,
  `docs/handoff-session-5-2026-08-16.md` (release-bump PR).
- **Tag v0.9.0**: anotado + GPG-signed, tag_object
  follow-up PR pattern (cf. v0.8.0 / PR #501).

---

Signed-off-by: opencode (sesión 5 closure)
