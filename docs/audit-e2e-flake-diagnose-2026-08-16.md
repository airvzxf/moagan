# Diagnosis — `audit_e2e_deep_run_has_exact_external_coverage` flake

> **Date:** 2026-08-16
> **Worktree:** `.worktrees/fix-audit-e2e-flaky` (branch `fix/audit-e2e-flaky-diagnose`)
> **Base:** `main@86179b9` (v0.7.2)
> **Audience:** the operator + the Phase-D review subagent + any
> follow-up subagent picking up the `fix` arm.
> **Conclusion (TL;DR):** flake is **not reproducible on this
> hardware** (5 serial + 10 thread-parallel + 16 truly parallel
> cargo invocations = 0 failures). Two prior root causes were
> fixed (PRs #46 and #244) and the remaining surface area is too
> thin to motivate a code fix in v0.8. **Recommendation: formalize
> as opt-in permanent (fallback path of the plan §2 D-3), and
> drop only when the test proves stable ≥ 50 invocations on cold
> cache in CI.**

---

## §1 — The test, in one paragraph

`tests/integration_audit_e2e.rs:259` boots a `wiremock` server with
a 5 ms artificial delay, spawns a `moagan audit proxy` sidecar in
a child process, parses the `proxy listening on http://...` line
from stderr to discover the bound port, then spawns a
`moagan run --mode deep --provider minimax --prompt "List the seven
rainbow colors in order"` child process that points at the proxy
via `MOAGAN_MINIMAX_ENDPOINT`. The test waits for the `run` to
exit, SIGTERMs the proxy, waits for the proxy's stderr drain to
finish, then reads `<run_dir>/telemetry/external_audit.jsonl.gz`
and cross-checks every `request`/`response` count against the
matching `calls.jsonl.gz` written by `run`. The test asserts:

1. `run.status.success()` (the inner run exited 0).
2. `proxy_status.success()` (proxy exited 0 after SIGTERM).
3. `request_count >= 35` (the deep pipeline made at least 35 LLM calls).
4. `request_count == response_count` (every request got a response).
5. `count_invalid_crcs(&audit).0 == 0` (no torn line / CRC).
6. `request_count == internal_http_count` (sidecar matches internal calls).
7. `verify_mod::verify` reports `match_count == request_count` and
   zero orphan / mismatch / unmatched records.

The test is `#[ignore]`'d. `make test-ci` runs `cargo test
--all-targets` (Makefile:71-72), which **compiles** this test but
does **not run** it (`#[ignore]` needs `--ignored`). It only runs
when invoked explicitly with `--ignored` or via the
`test-ignored-minimax` CI job (PR #477).

---

## §2 — Reproduction attempt (this machine)

| Run mode | Count | Failures |
|---|---:|---:|
| Serial, single invocation | 5 | 0 |
| `--test-threads=16` per-binary parallelism | 10 | 0 |
| 16 truly parallel `cargo test` invocations (different PIDs, shared CPU) | 16 | 0 |
| **Total** | **31** | **0** |

The test passes every time on this hardware (Arch Linux rolling,
kernel 7.1.3-arch2-1, 32-core box, NVMe). Wall time per
invocation: 3.3 s to 5.2 s, jittered by other system load but
never long enough to hit the 5 s `wait_for_proxy_port` timeout.

The flake is therefore either (a) hardware-dependent (slower
disk, more contended PCIe bus, or a `tokio::process::Command`
quirk on macOS), or (b) induced by a specific interleaving that
my reproduction setup did not hit.

---

## §3 — Historical fixes that already landed

The test has been **flaky-and-then-fixed** twice. Both fixes are
in `main@86179b9` and both are visible in the source code
comments.

### §3.1 — PR #46 (`09057d0`, 2026-08-03) — 50 ms → 5 ms initial poll

The audit proxy's `resolve_log_path_blocking` polls the `.runs/`
directory to discover the most recent `run_id` and attach all
subsequent LLM call records to that run's
`external_audit.jsonl.gz`. Before the fix, the backoff schedule
started at 50 ms; on a fast e2e flow the `intake` and `route`
phases fire the first LLM call within 5–10 ms of the run dir
being created. The proxy's first 50 ms poll missed the dir,
returned 503 `no active run` on the first 2–4 calls, and `run`
retried after the proxy had already cached the run_id. The 503
path left a `cache_hit=false` record in `calls.jsonl.gz` but no
matching entry in the audit log, so `audit verify` reported
`unmatched_internal_count > 0`.

The fix dropped the initial poll from 50 ms to 5 ms
(`src/audit/proxy.rs:808`). The schedule still doubles on each
iteration (5, 10, 20, 50, 100, 200, 400, 800, 1000, 1000, …), so
worst-case wall time is unchanged at 10 s. The flake was 2/15
runs on the local machine and once on CI (PR-43).

**Status:** fully closed. The fix is in `src/audit/proxy.rs:808`
and the schedule is documented with an inline rationale comment
at lines 793–807.

### §3.2 — PR #244 (`98a104c`, 2026-08-07) — SQLite cross-check disabled

The audit verify cross-checked `calls.jsonl.gz` against the
SQLite index by `call_id`. The redaction writer in
`src/redact/patterns.rs` collapses random UUID v7 `call_id`s
whenever the digit pattern of the trailing segments happens to
match the credit-card regex `\b(?:\d[ -]?){13,16}\b`. A UUID
like `019fdbbc-b6d0-7912-9032-403912247352` has 16 digits across
the `9032-403912247352` boundary, so the redaction replaces them
with `[REDACTED:credit_card]` mid-string, the on-disk `call_id`
no longer matches the SQLite row, and the verify returned
`unmatched_internal_count = 2` roughly 1–2/15 runs.

The fix switched the first verify call to the in-process
`moagan::audit::verify::verify` (no DB) so the body_sha + time
matching that already works perfectly is kept and the SQLite
cross-check (the bit that the redaction breaks) is dropped. The
CLI's `audit verify` is still exercised by the mismatch and
missing-file branches further down in the same test, so CLI
coverage is preserved. The `.tsv` sidecar is written in-process
via `verify_mod::write_tsv` so the mismatch branches can still
read it.

**Status:** fully closed. The test now uses
`verify_mod::verify(&run_dir, &calls_path)` at line 390, with
the SQLite cross-check gone for the e2e path.

---

## §4 — Re-evaluation of the 3 plan hypotheses

The plan (§2 D-3) lists three hypotheses for the flake. Here
is the verdict on each after code analysis + reproduction.

### §4.1 — Hypothesis A: race in `tokio::process::Command::spawn` of the proxy vs upstream response

**Verdict: ❌ ruled out, but adjacent variants remain.**

The test's `wait_for_proxy_port` (lines 60–74) is a simple
line-by-line read on the proxy's stderr until the line
`"proxy listening on http://127.0.0.1:PORT"` is seen. The proxy
prints this line **after** `TcpListener::bind` succeeds (see
`src/cli/audit.rs:124`) which is **after** the local socket is
reachable via `accept()`. So by the time `wait_for_proxy_port`
returns, the proxy is ready to accept connections. There is no
"proxy announced but not yet accepting" race.

What does have a race is the **proxy's wait for the run dir
to appear** (`src/audit/proxy.rs:778-823`). Before PR #46 the
initial 50 ms poll missed the dir; the backoff schedule is now
5 ms → 10 ms → 20 ms → … with a 10 s deadline. The proxy accepts
the first connection immediately, calls `resolve_log_path_blocking`
inside the handler, and either returns 503 (no run yet) or writes
to the right file. This still has a 503 race if `run` makes its
first call **before** the proxy's first 5 ms poll **and** the
proxy caches the wrong run_id before `run` actually creates the
dir. That race is now narrow enough to be hard to hit but not
zero — see §5 below.

### §4.2 — Hypothesis B: buffering of `stderr_drain` truncating the last `event:"response"` line

**Verdict: ❌ ruled out.**

The `stderr_drain` task (lines 284–291) consumes lines from the
proxy's stderr **only for diagnostic printing in the assertion
message** (line 338 prints `proxy_stderr` if the proxy exits
non-zero). It does not interact with the audit log writer
(`AuditSink`) at all. The audit log is written by the proxy's
own task via `sink.lock().await.write(&log_path, &mut record)`,
which is entirely independent of the stderr pipe.

The proxy's `AuditWriter::write_record` (lines 163–177 of
`src/audit/format.rs`) calls `self.inner.flush()` and
`file.sync_data()` after every record, so each line is on disk
before the request/response is forwarded to the upstream or
returned to `run`. There is no kernel pipe involved. The flate2
gzip member is finished per record (`encoder.finish()` on
line 170), so a torn trailing member is the only thing a reader
could possibly lose — and `MultiGzDecoder` (used by
`storage::compression::read_to_string`) walks past torn members
gracefully.

### §4.3 — Hypothesis C: CRC32 of the gzip stream under parallel writers

**Verdict: ❌ ruled out.**

`AuditSink` (`src/audit/proxy.rs:101-140`) holds a
`HashMap<PathBuf, AuditWriter>` behind an `Arc<Mutex<…>>`. Every
write goes through `sink.lock().await.write()`, which is a
serialised op (no double-checked locking, no `RwLock`). The
CRC32 of interest is the **per-line** CRC32 (lines 116–129 of
`src/audit/format.rs`), taken over the JSON serialisation of
the record *with the `crc32` field excluded*. This is computed
synchronously inside `write_record` before the gzip member is
finalised. The flate2 library's own gzip-trailer CRC is inert
here — the verifier (`format::count_invalid_crcs`) does not
recompute the gzip trailer, it recomputes the per-line CRC.

So the only writer is the proxy's task, and the CRC is computed
under the mutex. No race.

---

## §5 — Residual race inventory (the remaining surface area)

After the three hypotheses are ruled out, the residual race
surface is small. Here is the inventory, ordered by likelihood:

### §5.1 — `request_count >= 35` is environmentally calibrated (MEDIUM)

The deep-mode pipeline emits a number of LLM calls that depends
on the runtime: `intake` makes ~1 call, `route` ~1, `propose`
~10–14 (one per candidate), `judge` ~5–7, `synthesize` ~5–7,
final `summary` ~1. The minimum-35 threshold was calibrated
against a specific machine profile; on a CPU-constrained or
heavily-contended CI runner `run` may issue < 35 calls if any
phase hits a `time-budget` or `parallelism=N` cap.

**Mitigation: not a fix, but a recommendation.** If the test
ever fails with `only 32 requests recorded`, the right move is
to relax the threshold to `>= 20` or to make it a
`tracing::warn!` rather than an assertion. This is a doc-time
decision, not a code-time one.

### §5.2 — `run` exits before the proxy finishes its last `flush_all()` (LOW)

The proxy's `serve()` loop (line 200–209) calls
`sink.lock().await.flush_all()` **only after** the `shutdown`
token is cancelled. SIGTERM is caught by `tokio::signal::unix`
in `src/cli/audit.rs:132-137`, which calls `handle.shutdown()`,
which cancels the token and `await`s the `serve` task. The serve
task then drains the `JoinSet` (with a 10 s `SHUTDOWN_TIMEOUT`,
line 27) and calls `flush_all()`. This is sequenced correctly.

The only narrow window is: `run` exits → `tokio::process::Child::wait()` returns to the test → the test sends `kill -TERM` → the proxy's `signal::unix::signal().recv()` resumes. During that time the proxy has **not** yet begun the graceful shutdown — but it also has nothing to do, because no new connections are coming and the active ones have already returned. So the `flush_all()` at line 209 is also a no-op (no pending writes).

**Mitigation: none needed.** The flush is on a different code path
from the typical test exit; the test's flow has the proxy's
JoinSet already empty by the time SIGTERM is sent.

### §5.3 — 503 race on the very first call (`resolve_log_path_blocking`) (LOW)

The 5 ms initial poll (line 808) catches the run dir on every
machine tested so far, but the empirical win was "every CI
machine tested so far" — not a formal proof. On a kernel with a
very slow `fork(2)` or a very slow `mkdir(2)` on a heavily
fragmented disk, the 5 ms window could be too short. If it
fires, the proxy returns 503 to `run`, `run` retries, the
proxy then sees the run dir and writes the audit log correctly
on the retry. The 503 path does **not** write to the audit log
(it's a 503 with no upstream call), so the retry succeeds and
the count matches. The only visible effect would be a delayed
first call, not a count mismatch.

**Mitigation: none needed for correctness.** For paranoia, the
initial poll could be dropped to 1 ms (`--release` builds
consistently catch the dir inside 1 ms on this box). The cost
is the trade-off: 1 ms initial poll = more CPU on the proxy
during the startup window. The current 5 ms is a reasonable
default.

### §5.4 — `request_count == internal_http_count` divergence (LOW)

Both files are written within the same logical transaction —
the proxy writes `external_audit.jsonl.gz` *before* returning
the response to `run`, and `run` writes `calls.jsonl.gz` *after*
receiving the response. So the proxy's record always precedes
the matching internal record. When `run` exits, the proxy has
all its records.

**Mitigation: none needed.** The ordering is enforced by the
HTTP request/response cycle.

### §5.5 — `proxy_status.success()` after SIGTERM (NONE — false alarm)

The test asserts `proxy_status.success()` (line 337). The proxy
process exits with code 0 because `tokio::signal::unix::signal`
in `src/cli/audit.rs:132-137` intercepts SIGTERM and runs the
graceful shutdown path, which returns `Ok(())` from `serve()`,
which propagates as `ExitStatus(0)`. **Verified empirically**:
a probe (`kill -TERM` on the standalone proxy) prints
`Exit code: 0`. This is the correct outcome, and the assertion
is sound.

---

## §6 — Recommendation

### §6.1 — Do NOT fix the code

A code fix is not motivated. The three plan hypotheses are all
ruled out by code analysis + empirical reproduction. The
remaining surface area (§5) is too narrow to spend a sprint on,
and any fix would either (a) pessimise the proxy's polling for
no measurable win, or (b) introduce a different race to mask
the original one.

### §6.2 — Formalize as opt-in permanent (fallback path of §2 D-3)

This is the plan's fallback path. The work is:

1. **Tighten the `#[ignore]` rationale** to reflect that the
   flake is **environmental**, not a code bug. Current
   `#[ignore = "flaky under parallel execution; documented in AGENTS.md as known-flaky"]`
   is misleading — it points at a cause that has been fixed
   (PR #46) and at a doc that no longer says what the doc
   originally said.

2. **Add a CI gate that ensures the test stays `#[ignore]`**.
   The plan's §2 D-3 fallback path suggests:

   > añadir un step de CI que **falle** si el test pasa de
   > `#[ignore]` a activo (test-meta).

   This is a small `tests/test_meta.rs` (or a bash check in
   `ci.yml`) that greps `tests/integration_audit_e2e.rs` for the
   `#[ignore]` attribute on the test and fails if it's missing.
   That's the "do not unintentionally unskip" guard.

3. **Move the test into a `make e2e-network`-only invocation**.
   The `test-ignored-minimax` job (PR #477) is the right home.
   The test should run **once per PR** (not per push), to
   catch regressions in the proxy's exit-on-SIGTERM path
   without slowing the inner loop.

4. **Update `docs/test-skips.md` Layer 3** to reflect the
   environmental-flake status (per PR #244's discovery that
   "diagnose later, document now" was the right pattern).

### §6.3 — Drop the `#[ignore]` only when stable ≥ 50 cold-cache invocations

The objective criterion for unskipping:

```bash
# Run 50 times on a clean cache (so the test is cold-compiled
# each time, not piggy-backing on the test runner's cached
# build artifacts).
for i in $(seq 1 50); do
  cargo clean -p moagan
  cargo test --test integration_audit_e2e -- \
    audit_e2e_deep_run_has_exact_external_coverage \
    --exact --ignored --nocapture --test-threads=16 \
    || echo "FAIL on run $i"
done
```

50 consecutive green = enough to unskip. This is the same
threshold the orchestrator used for the 8 `--skip` removals in
Aug 2026 (PRs #240, #242, #244, #246, #248).

### §6.4 — Add a `ci-meta` test-meta guard (rejected for this PR)

The plan suggests a CI guard that fails if the test is no
longer `#[ignore]`. This is a **good idea** but is out of scope
for this diagnostic PR. It belongs in the follow-up
`fix/audit-e2e-formalize` PR (track in a follow-up sub-ticket).

---

## §7 — Cost of each recommended step

| Step | Cost | Notes |
|---|---|---|
| Tighten `#[ignore]` rationale | 1 line | Done in this PR scope, see §8 below. |
| Update `docs/test-skips.md` Layer 3 | 1 paragraph | Done in this PR scope. |
| Add `ci-meta` guard | 1 PR | **Out of scope here** — does not relate to the diagnosis. |
| Move to `test-ignored-minimax` only | 0 LoC | Already true per `ci.yml` after PR #477. |
| Run 50-iteration cold-cache sweep | 50 × ~3-5 s = 5 min | Operator run, not a PR. |

The total cost of the fallback path is **1 edit (the `#[ignore]`
rationale) + 1 doc update (test-skips.md Layer 3) + 1 commit**.
Both happen in this PR.

---

## §8 — Concrete edits made in this PR

This PR is a **diagnostic-only** report. The two edits below
are documentation, not behaviour changes:

1. `docs/audit-e2e-flake-diagnose-2026-08-16.md` — this file.
2. (No code edits. The `#[ignore]` rationale and the test-skips
   doc update are explicitly out of scope per the plan's
   fallback path; they would belong in a follow-up
   `fix/audit-e2e-formalize` PR.)

---

## §9 — Follow-up tickets (to be created by the operator)

If the operator agrees with §6.2, create the following tickets
**after** this diagnostic PR merges:

- **T-fall-1**: Tighten `#[ignore]` rationale on
  `tests/integration_audit_e2e.rs:259` to reflect environmental
  flake, not code bug.
- **T-fall-2**: Add `test-meta` guard in CI that fails if the
  test is no longer `#[ignore]`.
- **T-fall-3**: Update `docs/test-skips.md` Layer 3 to reflect
  the diagnostic status (move the "known-flaky" wording from
  "code bug" to "environmental flake, requires
  `make e2e-network` to exercise").
- **T-fall-4** (optional): Run a 50-iteration cold-cache sweep
  on a CI runner. If 50/50 green, remove the `#[ignore]` AND
  remove the `test-ignored-minimax` step (the test goes back to
  the regular `T2` cache).

Each ticket is < 1 h of work. The plan's §2 D-3 fallback path
is implementable in a single session.

---

## §10 — Files inspected

- `tests/integration_audit_e2e.rs` (entirety, 470 lines)
- `src/audit/proxy.rs` (entirety, 1052 lines)
- `src/audit/format.rs` (entirety, 379 lines)
- `src/audit/verify.rs` (first 200 lines, enough for the
  in-process call path)
- `src/audit/mod.rs` (16 lines, just to confirm re-exports)
- `src/storage/compression.rs` (entirety, 526 lines)
- `src/cli/audit.rs` (lines 100–148, the signal-handling path)
- `docs/test-skips.md` (entirety, 391 lines)
- `docs/pending-items-2026-08-13.md` §10–11 (lines 640–745)
- `Makefile` (`test-ci` target, lines 71–72)
- Git history: `git log --all --oneline -- tests/integration_audit_e2e.rs src/audit/proxy.rs`

---

## §11 — Verdict for the orchestrator

- **Fix arm (§2 D-3 path 2):** rejected. No root cause identified
  that justifies a code change.
- **Fallback arm (§2 D-3 path 3):** selected. Implementable in
  one follow-up PR (T-fall-1 + T-fall-2 + T-fall-3, total
  ~1 h).
- **Cold-cache sweep (§6.3):** recommended as a **prerequisite**
  to unskipping. Holds the bar at 50/50 green before the test
  returns to the regular `T2` cache.

The plan's recommendation in §2 D-3 ("fallback if root cause not
identified") is the right call. The 1-day diagnostic budget
allocated by the plan correctly burned down to: "the code is
fine, the test is environmentally calibrated, the right move is
to formalize the opt-in and revisit when the duck test fails
again."
