SHELL := /usr/bin/env bash
.SHELLFLAGS := -euo pipefail -c

# Auto-enable sccache if installed; CI/dev environments without it are unaffected
SCCACHE := $(shell command -v sccache 2>/dev/null)
ifneq ($(SCCACHE),)
export RUSTC_WRAPPER := sccache
endif

.DEFAULT_GOAL := help

.PHONY: help setup build release install fmt fmt-check check lint lint-ci test test-unit test-contract test-e2e test-e2e-portable test-e2e-unix test-doc test-inventory test-all doc coverage coverage-unit coverage-e2e coverage-total deny ci run tui clean

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_-]+:.*## / {printf "  %-14s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

setup: ## Install rustfmt, Clippy, cargo-llvm-cov, and cargo-deny
	./scripts/bootstrap.sh

build: ## Build the complete workspace
	cargo build --workspace --all-features --locked

release: ## Build the latte-code release binary
	cargo build --release -p latte-code --locked

install: ## Install latte-code from the local checkout
	cargo install --path crates/latte-code --locked

fmt: ## Format Rust source
	cargo fmt --all

fmt-check: ## Check Rust formatting
	cargo fmt --all -- --check

check: ## Run formatting, compilation, Clippy, and documentation checks
	./scripts/check.sh

lint: ## Run Clippy with warnings denied
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

lint-ci: ## Run actionlint and ShellCheck (uses local tools or pinned Docker images)
	./scripts/lint-ci.sh

test: test-all ## Run every required test layer

test-unit: ## Run crate-local unit tests
	cargo test --workspace --lib --bins --all-features --locked

test-contract: ## Run public contract and component integration targets
	cargo test -p latte-core --test contracts --all-features --locked
	cargo test -p latte-engine --test public_lifecycle --all-features --locked
	cargo test -p latte-code --test architecture --test contract --test markdown_links --all-features --locked

test-e2e: test-e2e-portable test-e2e-unix ## Run every final-binary E2E suite on Unix

test-e2e-portable: ## Run portable final-binary headless/Provider/SQLite E2E
	cargo test -p latte-code --test e2e_portable --all-features --locked

test-e2e-unix: ## Run Unix-only final-binary PTY/process E2E
	cargo test -p latte-code --test e2e_unix --all-features --locked -- --test-threads=1

test-doc: ## Run workspace documentation tests
	cargo test --workspace --doc --all-features --locked

test-inventory: ## Verify every integration target belongs to a required layer
	./scripts/test-inventory.sh

test-all: ## Run inventory, unit, contract, E2E, and documentation tests
	./scripts/test.sh

doc: ## Build local API documentation
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

coverage: coverage-unit coverage-e2e coverage-total ## Run every required coverage gate
	rm -rf target/llvm-cov-target coverage lcov.info

coverage-unit: ## Run UT-only coverage with the 95% line gate
	cargo llvm-cov --workspace --all-features --lib --bins --locked --fail-under-lines 95

coverage-e2e: ## Run final-binary E2E coverage with the 80% line gate
	cargo llvm-cov --workspace --all-features --test e2e_portable --test e2e_unix --locked --fail-under-lines 80 -- --test-threads=1

coverage-total: ## Run all-target coverage with the 90% line gate
	cargo llvm-cov --workspace --all-features --all-targets --locked --fail-under-lines 90 -- --test-threads=1

deny: ## Audit advisories, licenses, bans, and sources
	cargo deny --locked check

ci: ## Reproduce the complete local CI gate
	./scripts/ci-local.sh

run: ## Run the headless CLI; pass arguments with ARGS='...'
	cargo run -p latte-code --locked -- $(ARGS)

tui: ## Start the Ratatui interface
	cargo run -p latte-code --locked -- tui

clean: ## Remove Cargo build and coverage output
	cargo llvm-cov clean --workspace 2>/dev/null || true
	cargo clean
	rm -rf coverage lcov.info
