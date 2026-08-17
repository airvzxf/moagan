# Prompt de handoff — `moagan` v0.9.1 work session (cierre de sesión 6)

> **Propósito**: arrancar una sesión nueva de opencode en
> `/home/wolf/workspace/projects/moagan` con el máximo contexto
> posible al cierre formal de la sesión 6 (v0.9.1 released,
> patch bump). Léeme entero antes de hacer cualquier otra cosa.

---

## 1. TL;DR

HEAD `1e696a1 ci(workflows): disable test-ignored-deepseek +
retry loop for fast/explore (#529)`. Local in sync con
`origin/main`. Tag `v0.9.1` anotado y pusheado (post-release-
bump merge). Crate version `0.9.1` (Cargo.toml). Test suite
verde: `cargo test --all-targets` reporta **2234 passed across
43 test binaries**, 0 failed, 6 `#[ignore]` (3 audit_e2e
documented flakes per #482 + 2 cgroup `prlimit` mutators que
requieren root + 1 pre-existing del v0.9.0 baseline).
**`cargo test --features dag --all-targets` reporta 2246
passed, 0 failed** (los +12 son los dag-gated tests de
`src/phases/dag.rs` + 5 integration feature-only en
`tests/integration_petgraph_dag.rs`, total 12 = 2246 − 2234).
**3 PRs substantivos** squash-mergeados en sesión 6 desde
`docs/handoff-session-5-2026-08-16.md` (HEAD `ebaaa3d` /
v0.9.0). Tracks cerrados en sesión 6: **K.4 fetcher PDF routing
wired** (#527 fetcher slice, cierre del follow-up de #518),
**HardIncompat detector wiring** (#527 constraint + synthesize
slices, cierre del follow-up de #504 para los 3 opt-in
variants `ClusterLocalInGlobal` / `PullInPushOnly` /
`StatelessInStateful`), y **CI reliability batch** (#525
restructure + cache cleanup; #529 DeepSeek stub + retry loop).
`docs/v0.9.1-final-report.md` documenta el catálogo completo.

---

## 2. Tabla de los 3 PRs substantivos mergeados (Sesión 6)

(Orden cronológico inverso — newest first.)

| # | PR | D.x / categoría | Capacidad |
|---:|---:|---|---|
| 1 | #529 | Z2 / CI infra | `test-ignored-deepseek.yml` stubbed DISABLED (DeepSeek budget exhausted, ver `docs/pending-items-2026-08-13.md §9.3`); `e2e-network.yml` `test-fast` y `test-explore` ahora envueltos en retry shell (4 attempts, 60 s backoff) |
| 2 | #527 | B#20 sub-1 follow-up + I.6 detector wiring / Feature | K.4 fetcher PDF routing (`src/research/fetcher.rs`, 3 unit) + HardIncompat opt-in detectors (`src/domain/constraint.rs` + `src/phases/synthesize.rs`, 7 unit + 3 integration) + `skipped_opt_in` sidecar |
| 3 | #525 | Z1 / CI infra | `ci.yml` restructured (4 jobs removed → post-merge workflows); `Swatinem/rust-cache@v2.9.2` `lookup-only: true` on PRs (cleans `refs/pull/N/merge` cache pollution); `e2e-network.yml` 5 timeouts bumped; `test-ignored-minimax` timeout 25 → 60 min |

---

## 3. Deferred items restantes (no atacados)

Ordenados por valor/costo descendente. El estado refleja HEAD
`1e696a1` y tag `v0.9.1` (2026-08-17).

### 3.1 CLI wire-up de `maybe_run_via_dag` cuando `mode == "deep"`

- El dispatcher está en `src/phases/pipe.rs` (#520 gated,
  +37 LoC). Sólo falta llamar desde
  `cli/run.rs::build_pipeline_for_mode` (~5 líneas).
- **Coste**: < 1 PR (S-sized).
- **Bloqueante actual**: valor 100% prospectivo mientras no
  se añadan fan-outs reales al DAG. Sin fan-outs no hay
  observable effect — el dispatcher queda inerte.

### 3.2 K.4 wire-up de `fetcher::fetch_one` para PDFs — ✅ DONE en sesión 6 (#527)

- Ahora `ResearchFetcher::fetch_one` rutea URLs con `.pdf` al
  parser `src/research/pdf.rs::fetch_pdf_text` después del
  allowlist gate. HTML URLs siguen por el path original sin
  cambio. 3 unit tests pinean el contrato.
- **Cerrado en #527 slice 3.1.**

### 3.3 HardIncompat detector wiring para los 3 nuevos variants — ✅ DONE en sesión 6 (#527)

- Las heurísticas de `ClusterLocalInGlobal`, `PullInPushOnly`,
  `StatelessInStateful` están ahora en
  `src/domain/constraint.rs` (+259 LoC), envueltas por
  `cluster_opt_in_hardincompat` (deterministic-order wrapper).
  `SynthesizePhase` las invoca vía el wrapper y emite el nuevo
  `skipped_opt_in` sidecar cuando filtra pre-jurado.
- 7 unit + 3 integration tests pinean el contrato end-to-end.
- **Cerrado en #527 slice 3.2.**

### 3.4 `process_locks` heartbeat sweep / GC

- El lease module `src/storage/lease.rs` está shippeado (#506)
  con `acquire_process_lock` / `heartbeat_process_lock` /
  `release_process_lock`. Falta un sweepper periódico que
  libere leases con `expires_at_unix` vencido.
- **Coste**: 1 PR (M-sized, ~1 d).
- **Bloqueante actual**: sin evidence de acumulación en
  telemetría; profilaxis sin urgencia.

### 3.5 K.4 sub-2 — JS rendering

- JS rendering (B#20 sub-2) NO atacado. Diferido a v0.10+ por
  coste (Chromium / Playwright > 100 MB).
- **Bloqueante actual**: decisión de scope primero — ¿el
  audience de docs que necesitamos ingesting es
  JS-rendered o estático? Responder antes de invertir XL.

### 3.6 GLM / Qwen / Kimi providers

- 3/3 providers sin código de producción, sólo entries de
  catálogo + asserts en `verify-spec-gaps §1.3`.
- Bloqueados por ticket del operador (decisión formalizada en
  `docs/deferred-v0-9-2026-08-16.md §1.1`).

### 3.7 Cross-platform sandbox fallback (macOS / WSL)

- Sandbox hardening (Tracks E + D.11.x) es Linux-only.
  `unshare(CLONE_NEW*)` + cgroup v2 + seccomp BPF no tienen
  equivalente en macOS / WSL.
- Decisión del operador formalizada en
  `docs/deferred-v0-9-2026-08-16.md §1.2`: único público
  objetivo = Linux puros.

### 3.8 `comfy-table` para tablas CLI

- ADR-0001 §D-2 difiere `comfy-table` a v0.9. **v0.9.0 y
  v0.9.1 cerraron sin adoptarlo**. Re-evaluar si surge
  use-case.

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
  `optional = true` (Cargo feature `dag`). Default build no
  lo pulla; `phases/` vector lineal sigue siendo el path por
  defecto. **Adoptado en v0.9 (#520)** con `topological_layers`
  (Kahn) + `execute_dag` (`futures::join_all`).
- **`comfy-table 7.1`** — DEFERRED. Sin adopt en v0.9 / v0.9.1.
  Texto plano funciona y los tests visuales con `insta`
  cubren regresiones de tabla.
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

### 4.5 K.4 PDF fetcher (DONE en sesión 6, #527)

El PDF parser (`src/research/pdf.rs::fetch_pdf_text`) está
ahora wired en `src/research/fetcher.rs::fetch_one`: las URLs
con `.pdf` (case-insensitive ASCII) se rutean al parser
después del allowlist gate. HTML URLs siguen por el path
original sin cambio.

Requiere `poppler-utils` instalado:

```bash
pacman -S poppler          # Arch
apt install poppler-utils  # Debian/Ubuntu
```

Sin `poppler-utils`, el fetcher retorna
`Error::ResearchUnavailable` con hint de install — no falla
en silencio.

### 4.6 HardIncompat opt-in catalog (DONE en sesión 6, #527)

Los 3 variants opt-in (`ClusterLocalInGlobal`,
`PullInPushOnly`, `StatelessInStateful`) ahora tienen
detectores activos en `SynthesizePhase`. La heurística vive en
`src/domain/constraint.rs` (`detect_cluster_local_in_global`
/ `detect_pull_in_push_only` /
`detect_stateless_in_stateful`) y se invoca vía
`cluster_opt_in_hardincompat(plan, base_detector) -> (fatal,
skipped_opt_in)`. Las propuestas con estas incompatibilidades
se filtran pre-jurado y se loguean en `final/skipped_opt_in.jsonl`
(sidecar nuevo).

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

## 6. Plan para la siguiente sesión (sesión 7)

Status al cierre de HEAD `1e696a1` + tag `v0.9.1` (2026-08-17):

- ✅ **K.4 fetcher PDF routing** (#527 slice 3.1) — DONE.
- ✅ **HardIncompat detector wiring** (#527 slice 3.2) — DONE.
- ✅ **CI reliability batch** (#525 Z1 + #529 Z2) — DONE.
- ✅ **v0.9.0 follow-ups cerrados** — todas las entradas
  marcadas en `docs/v0.9-final-report.md §5` como "the natural
  follow-up" (K.4 fetcher + HardIncompat detectors) atacadas
  en #527.

Tracks abiertos al cierre (ordenados por valor/costo):

### 6.1 CLI wire-up de `maybe_run_via_dag`

- S-sized (1 PR, ~5 líneas).
- `cli/run.rs::build_pipeline_for_mode` debe llamar
  `maybe_run_via_dag` cuando `mode == "deep"` y
  `cfg!(feature = "dag")`.
- El follow-up queda explícito en el body de #520.
- **Pre-condición**: si se quiere ROI observable, añadir
  primero un fan-out real al DAG (sino el dispatcher queda
  inerte). Candidato si el operador quiere usar la feature
  `dag` para cluster en `mode == "deep"`.

### 6.2 `process_locks` heartbeat sweep / GC

- M-sized (1 PR, ~1 d).
- Sweepper periódico (background task) que libera leases con
  `expires_at_unix` vencido. Necesita `CancellationToken`
  parent per no-go list.
- **Pre-condición**: evidencia de acumulación en telemetría
  (leases que sobreviven a su deadline esperado). Sin
  evidencia, defer.

### 6.3 K.4 sub-2 — JS rendering (decisión de scope)

- XL (5+ d).
- 1 h de **decisión de scope antes de invertir XL**: ¿el
  audience objetivo de docs es JS-rendered (entonces
  Playwright / Chromium > 100 MB) o estático (entonces
  `pdftotext`+`w3m`+`links -dump` ya cubren la mayoría)?
- Si la respuesta es "mayoría estático", defer indefinido.

### 6.4 Deferred a v0.10+

- GLM/Qwen/Kimi providers (bloqueados por ticket del
  operador).
- Cross-platform sandbox (bloqueado por decisión del
  operador).
- `comfy-table` para CLI (re-evaluar ADR-0001 §D-2).
- Multimodal streaming (bloqueado upstream).

---

## Anexo A — Topología del código (recordatorio post-v0.9.1)

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
│                                # constraint (HardIncompat + 3 opt-ins + 
│                                # detectors wired, #504 + #527)
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
│                                # 28 roles
├── modal_gate.rs
├── phases/                      # 28 archivos, fase por fase del pipeline
│                                # (+dag.rs gated, +maybe_run_via_dag gated, #520)
├── preferences/
├── ranking/                     # pareto + diversity + cluster + rubric + stability
├── reconcile/
├── redact/                      # RedactPolicy + patterns + writer
├── research/                    # allowlist + fetcher (K.4 fetcher PDF routing
│                                # wired, #527) + auth (#498) + host_policy +
│                                # circuit_breaker + pdf (pdftotext, #518)
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

Cambios netos en v0.9.1 (sesión 6):

- `src/research/fetcher.rs` (+171 LoC) — K.4 fetcher PDF
  routing.
- `src/domain/constraint.rs` (+259 LoC) — 3 detector functions
  + `cluster_opt_in_hardincompat` wrapper.
- `src/phases/synthesize.rs` (+297 LoC) — `cluster_opt_in`
  invocation + `skipped_opt_in` sidecar writer.

---

## Anexo B — Documentos normativos (orden de lectura)

1. `docs/proposal-01-concept.md` — visión conceptual (V4).
2. `docs/proposal-02-rust.md` — spec técnica (T01-06).
3. `docs/proposal-03-add-ons.md` — catálogo aditivo (D.x),
   con v0.9.1 markers (K.4 fetcher routing wired, HardIncompat
   opt-in detectors wired, K.4 sub-1 fully done).
4. `docs/proposal-04-cuarta-etapa.md` — cuarta etapa
   (K.4 sub-1 DONE end-to-end, sub-2 deferred).
5. `docs/v0.8-final-report.md` — v0.8.0 final report
   (predecessor).
6. `docs/v0.9-final-report.md` — v0.9.0 final report
   (predecesor inmediato).
7. `docs/v0.9.1-final-report.md` — v0.9.1 final report (este
   release).
8. `docs/deferred-v0-9-2026-08-16.md` — v0.9 NOT-DO list
   (formalizado por #502; honoured por v0.9.0 y v0.9.1).
9. `docs/handoff-next-session.md` — handoff histórico (v0.4;
   superseded por este documento).
10. `docs/handoff-session-4-2026-08-16.md` — handoff al cierre
    de sesión 4.
11. `docs/handoff-session-5-2026-08-16.md` — handoff al cierre
    de sesión 5 (v0.9.0 release).
12. `docs/handoff-session-6-2026-08-17.md` — este archivo.
13. `docs/adr/0001-no-go-list-policy.md` — ADR-0001
    (differentiated no-go list policy; honrado por v0.9 +
    v0.9.1).
14. `docs/inconsistencies-audit-2026-08-12*.md` — auditorías
    de inconsistencias (rounds 1-12).
15. `AGENTS.md` — convenciones + no-go list actualizada con
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
- **poppler-utils**: requerido para K.4 PDF fetcher (#518 +
  wired #527). `pacman -S poppler` (Arch) /
  `apt install poppler-utils` (Debian/Ubuntu).

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
git ls-remote origin refs/tags/v0.9.1

# Verificar que pdftotext está disponible (K.4 #518 / wired #527)
which pdftotext
```

---

## Anexo D — Resumen de cambios v0.9.0 → v0.9.1

- **3 PRs substantivos** mergeados (#525, #527, #529).
- **Cargo.toml**: `0.9.0 → 0.9.1`.
- **17 fixtures** `register_run(..., "0.9.0", ...)` →
  `"0.9.1"` en `src/storage/sqlite.rs`,
  `src/telemetry/dashboard.rs`,
  `tests/integration_phase_i.rs`,
  `tests/integration_telemetry_saturation.rs`.
- **K.4 fetcher PDF routing** (+171 LoC en
  `src/research/fetcher.rs`, 3 unit tests).
- **HardIncompat detector wiring** (+259 LoC en
  `src/domain/constraint.rs`, +297 LoC en
  `src/phases/synthesize.rs`, 7 unit + 3 integration tests).
- **`skipped_opt_in.jsonl`** sidecar writer (nuevo, en
  `final/`).
- **CI restructure**: `ci.yml` 4 jobs moved to post-merge;
  `Swatinem/rust-cache@v2.9.2` `lookup-only` on PRs; 5
  timeouts bumped en `e2e-network.yml`; `test-ignored-minimax`
  timeout 25 → 60 min.
- **DeepSeek stub**: `test-ignored-deepseek.yml` reemplazada
  por stub DISABLED (38 líneas).
- **Retry loop**: `test-fast` + `test-explore` en
  `e2e-network.yml` envueltos en shell retry (4 attempts,
  60 s backoff).
- **0 new runtime deps**.
- **0 schema migrations**.
- **0 breaking changes**.
- **2 new docs**: `docs/v0.9.1-final-report.md`,
  `docs/handoff-session-6-2026-08-17.md` (release-bump PR).
- **Tag v0.9.1**: anotado + GPG-signed, tag_object
  follow-up PR pattern (cf. v0.8.0 / PR #501, v0.9.0 /
  PR #523).

---

Signed-off-by: opencode (sesión 6 closure, v0.9.1 patch release)
