# Moagan validation gauntlet.
# Usage: make validate
#
# Validation tiers (full rationale: AGENTS.md §"Validation tiers"):
#
#   T0  sub-second    fmt-check + guard-deps                → pre-commit (parallel)
#   T1  seconds       lint + build                          → pre-commit (parallel)
#   T2  minutes       test-ci (cargo test --all-targets)    → pre-push
#   T3  CI only       smoke + e2e; e2e-network on main only → .github/workflows/
#   msg <1 s          conventional commit subject check     → commit-msg
#
# Dev loop (compact):
#
#   $ edit src/...                                  (T0 + T1 in ~30-90 s, parallel)
#   $ git commit -m "feat(llm): add streaming parser"
#      └─► commit-msg     ─ check format ─── pass
#      └─► pre-commit     ─ T0: fmt-check + guard-deps ── parallel
#                          T1: lint + build ──────────── parallel
#   $ git push                                       (T2: cargo test in 1-5 min)
#      └─► pre-push       ─ T2: cargo test ─── sequential
#
#   GitHub Actions — ci.yml (8 required, ~6 min cold / ~3 min warm):
#      round 1: fmt-check  │  guard-deps  │  clippy
#      round 2: test-lib  │  test-tests  │  test-doc  │  smoke  │  e2e
#
#   GitHub Actions — e2e-network.yml (post-merge, main only, ~15 min cold):
#      preflight-minimax  →  build-e2e-network
#      →  fast (~2 min)  +  explore (~8 min)  in parallel
#
# Smoke vs E2E:
#   smoke_*  Pure-static checks that grep the source, read static
#            SQLite, or run grep/jq over already-produced artefact
#            files. Runs in <2 s total.
#   e2e_*    End-to-end checks that actually launch `moagan run`
#            (some against the mock provider in <30 s, some against
#            the real minimax upstream taking ~25 min). Opt-in via
#            `make e2e` so an inner loop never pays that cost.

.DEFAULT_GOAL := help

SMOKE_SCRIPTS := \
	scripts/smoke_intra_cluster_synthesis.sh \
	scripts/smoke_adversary_judge.sh \
	scripts/smoke_human_checkpoint.sh \
	scripts/smoke_checkpoint_mirror.sh \
	scripts/smoke_phase_d_integration.sh \
	scripts/smoke_preflight.sh

E2E_SCRIPTS_LOCAL := \
	scripts/e2e_pipeline_modes.sh \
	scripts/e2e_interactive_checkpoints.sh

E2E_SCRIPTS_NETWORK := \
	scripts/e2e_audit_proxy.sh

.PHONY: help validate fmt fmt-check lint test test-doc build build-release doc clean check-deps guard-deps smoke e2e e2e-fast e2e-network e2e-network-card80 e2e-network-fast e2e-network-explore e2e-network-discover-opencode e2e-network-discover-deepseek e2e-network-discover-opencode-models smoke-audit profile-build profile-clean

# Per-branch compile-time profiling. See AGENTS.md §"Validation tiers"
# for rationale and ADR-0007 §"Measured costs" for the baseline.
PROFILE_BRANCH := $(shell git rev-parse --abbrev-ref HEAD 2>/dev/null | tr '/' '-' | tr 'A-Z' 'a-z' || echo unknown)
PROFILE_DIR   := target/timings/$(PROFILE_BRANCH)
PROFILE_TARGETS := leaf middle hub

help:
	@echo "Targets:"
	@echo "  validate        - Run fmt-check, lint, test, build, smoke (the Gauntlet)"
	@echo "  fmt             - Auto-format the codebase"
	@echo "  fmt-check       - Verify formatting"
	@echo "  lint            - Run clippy with -D warnings"
	@echo "  test            - Run cargo test --all-targets"
	@echo "  test-doc        - Run cargo test --doc"
	@echo "  build           - Debug build"
	@echo "  build-release   - Release build"
	@echo "  doc             - Build documentation"
	@echo "  guard-deps      - Run forbidden-crate and SDK guards"
	@echo "  smoke           - Run all 5 fast smoke_* suites (default in validate)"
	@echo "  e2e             - Run local e2e_* suites (mock pipeline, ~1 min)"
	@echo "  e2e-fast        - Same as 'smoke' (alias kept for discoverability)"
	@echo "  e2e-network     - Run e2e_audit_proxy.sh (real LLM, up to 35 min)"
	@echo "  e2e-network-card80 - Run only the card80 sub-block (real LLM, ~25 min)"
	@echo "  e2e-network-fast   - Run only the mode-fast sub-block (real LLM, ~2 min)"
	@echo "  e2e-network-explore - Run only the mode-explore sub-block (real LLM, ~8 min)"
	@echo "  e2e-network-discover-opencode       - Run only the opencode discovery sub-block (real LLM, ~10 min)"
	@echo "  e2e-network-discover-deepseek       - Run only the deepseek discovery sub-block (real LLM, ~20 min)"
	@echo "  e2e-network-discover-opencode-models - Run the 7-model opencode coverage loop (real LLM, ~35 min)"
	@echo "  smoke-audit     - Run smoke_audit_proxy.sh standalone (~1 min)"
	@echo "  profile-build   - Run \`cargo build --timings\` on leaf/middle/hub scenarios; persist HTML to target/timings/<branch>/"
	@echo "  profile-clean   - Remove target/timings/"
	@echo "  clean           - Remove target/"

validate: fmt-check guard-deps lint test build smoke
	@echo "OK: validate passed"

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets -- -D warnings

test:
	MOAGAN_NON_INTERACTIVE=1 cargo test --all-targets

test-ci:
	MOAGAN_NON_INTERACTIVE=1 cargo test --all-targets

test-doc:
	MOAGAN_NON_INTERACTIVE=1 cargo test --doc

build:
	cargo build

build-release:
	cargo build --release

doc:
	cargo doc --no-deps

guard-deps:
	bash scripts/check-no-anthropic-sdk.sh
	bash scripts/check-no-forbidden-crates.sh
	bash scripts/check-no-trace-debug-in-mod-tests.sh
	bash scripts/check-non-interactive-env-guard.sh
	bash scripts/check-changelog-release.sh

smoke:
	@echo ">>> Running 5 smoke suites (fast, <2 s total)…"
	@for s in $(SMOKE_SCRIPTS); do echo ""; echo "=== $$s ==="; bash $$s || exit 1; done
	@echo ""
	@echo "OK: smoke passed"

smoke-audit:
	@echo ">>> Running smoke_audit_proxy.sh (long discover skipped)…"
	@MOAGAN_SMOKE_LONG_DISCOVER=1 bash scripts/smoke_audit_proxy.sh || exit 1

e2e:
	@echo ">>> Running local e2e suites (mock pipeline, ~1 min)…"
	@for s in $(E2E_SCRIPTS_LOCAL); do echo ""; echo "=== $$s ==="; bash $$s || exit 1; done
	@echo ""
	@echo "OK: e2e passed"

e2e-fast: smoke

e2e-network:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM; set MOAGAN_SMOKE_LONG_DISCOVER=1 to skip the 25-min card80 block)…"
	@bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-card80:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, card80 only, ~25 min)…"
	@MOAGAN_SMOKE_SECTION=card80 bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-fast:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, mode fast only, ~2 min)…"
	@MOAGAN_SMOKE_SECTION=fast bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-explore:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, mode explore only, ~8 min)…"
	@MOAGAN_SMOKE_SECTION=explore bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-discover-opencode:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, discover_opencode block only)…"
	@MOAGAN_SMOKE_SECTION=discover_opencode bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-discover-deepseek:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, discover_deepseek block only)…"
	@MOAGAN_SMOKE_SECTION=discover_deepseek bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-discover-opencode-models:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, opencode per-model coverage loop)…"
	@MOAGAN_SMOKE_SECTION=discover_opencode_models bash scripts/e2e_audit_proxy.sh || exit 1

clean:
	cargo clean

# Per-branch compile-time profiling. Touches one source file per
# scenario, runs `cargo build --timings`, and copies the resulting
# HTML timing report to `target/timings/<branch>/<scenario>.html`.
#
# Scenarios (named in increasing fan-out):
#   leaf    — src/llm/openai_compat.rs  (provider leaf, ~14 file deps)
#   middle  — src/cli/discover.rs       (orchestrator, ~25 file deps)
#   hub     — src/phases/phase.rs       (kernel, 25+ direct deps)
#
# The HTML files are gitignored at `target/timings/<branch>/`; the
# `target/timings/main/` baseline is the only committed copy and is
# shipped as a one-time reference for ADR-0007 §"Measured costs".
profile-build: $(addprefix $(PROFILE_DIR)/,$(addsuffix .html,$(PROFILE_TARGETS)))

$(PROFILE_DIR)/leaf.html:
	@mkdir -p $(PROFILE_DIR)
	@touch src/llm/openai_compat.rs
	@cargo build --timings 2>&1 | tail -1
	@cp $$(ls -t target/cargo-timings/cargo-timing-*.html | head -1) $@
	@echo "Wrote $@"

$(PROFILE_DIR)/middle.html:
	@mkdir -p $(PROFILE_DIR)
	@touch src/cli/discover.rs
	@cargo build --timings 2>&1 | tail -1
	@cp $$(ls -t target/cargo-timings/cargo-timing-*.html | head -1) $@
	@echo "Wrote $@"

$(PROFILE_DIR)/hub.html:
	@mkdir -p $(PROFILE_DIR)
	@touch src/phases/phase.rs
	@cargo build --timings 2>&1 | tail -1
	@cp $$(ls -t target/cargo-timings/cargo-timing-*.html | head -1) $@
	@echo "Wrote $@"

profile-clean:
	rm -rf target/timings/
