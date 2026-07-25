# Moagan validation gauntlet.
# Usage: make validate

.DEFAULT_GOAL := help

.PHONY: help validate fmt fmt-check lint test test-doc build build-release doc clean check-deps guard-deps

help:
	@echo "Targets:"
	@echo "  validate       - Run fmt-check, lint, test, build (the Gauntlet)"
	@echo "  fmt            - Auto-format the codebase"
	@echo "  fmt-check      - Verify formatting"
	@echo "  lint           - Run clippy with -D warnings"
	@echo "  test           - Run cargo test --all-targets"
	@echo "  test-doc       - Run cargo test --doc"
	@echo "  build          - Debug build"
	@echo "  build-release  - Release build"
	@echo "  doc            - Build documentation"
	@echo "  guard-deps     - Run forbidden-crate and SDK guards"
	@echo "  clean          - Remove target/"

validate: fmt-check guard-deps lint test build
	@echo "OK: validate passed"

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test --all-targets

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

clean:
	cargo clean
