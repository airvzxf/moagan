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
	scripts/smoke_phase_d_integration.sh

E2E_SCRIPTS_LOCAL := \
	scripts/e2e_pipeline_modes.sh \
	scripts/e2e_interactive_checkpoints.sh

E2E_SCRIPTS_NETWORK := \
	scripts/e2e_audit_proxy.sh

.PHONY: help validate fmt fmt-check lint test test-doc build build-release doc clean check-deps guard-deps smoke e2e e2e-fast e2e-network smoke-audit

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
	@echo "  e2e-network     - Run e2e_audit_proxy.sh (real LLM, up to 25 min)"
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
	cargo test --all-targets

test-ci:
	cargo test --all-targets -- \
		--skip audit_e2e_deep_run_has_exact_external_coverage \
		--skip llm::response_format_opt_out::tests::env_var_extends_opt_out

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

clean:
	cargo clean
