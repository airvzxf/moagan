# Moagan validation gauntlet.
# Usage: make validate
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

.PHONY: help validate fmt fmt-check lint test test-doc build build-release doc clean check-deps guard-deps smoke e2e e2e-fast e2e-network e2e-network-card80 e2e-network-fast e2e-network-explore e2e-network-discover-opencode-go e2e-network-discover-deepseek e2e-network-discover-opencode-go-models smoke-audit

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
	@echo "  e2e-network-discover-opencode-go - Run only the opencode_go discovery sub-block (real LLM, ~20 min)"
	@echo "  e2e-network-discover-deepseek    - Run only the deepseek discovery sub-block (real LLM, ~20 min)"
	@echo "  e2e-network-discover-opencode-go-models - Run the 13-model opencode_go coverage loop (real LLM, ~65 min)"
	@echo "  smoke-audit     - Run smoke_audit_proxy.sh standalone (~1 min)"
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
	cargo test --doc

build:
	cargo build

build-release:
	cargo build --release

doc:
	cargo doc --no-deps

guard-deps:
	bash scripts/check-no-anthropic-sdk.sh
	bash scripts/check-no-forbidden-crates.sh

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

e2e-network-discover-opencode-go:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, discover_opencode_go block only)…"
	@MOAGAN_SMOKE_SECTION=discover_opencode_go bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-discover-deepseek:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, discover_deepseek block only)…"
	@MOAGAN_SMOKE_SECTION=discover_deepseek bash scripts/e2e_audit_proxy.sh || exit 1

e2e-network-discover-opencode-go-models:
	@echo ">>> Running e2e_audit_proxy.sh (REAL LLM, opencode_go per-model coverage loop)…"
	@MOAGAN_SMOKE_SECTION=discover_opencode_go_models bash scripts/e2e_audit_proxy.sh || exit 1

clean:
	cargo clean
