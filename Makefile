SHELL := /usr/bin/env bash
.SHELLFLAGS := -euo pipefail -c
.DEFAULT_GOAL := help

.PHONY: help setup build release install fmt fmt-check check lint test doc coverage deny ci run tui clean

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_-]+:.*## / {printf "  %-14s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

setup: ## Install rustfmt, Clippy, cargo-llvm-cov, and cargo-deny
	./scripts/bootstrap.sh

build: ## Build the complete workspace
	cargo build --workspace --all-features

release: ## Build the lattecode release binary
	cargo build --release -p lattecode

install: ## Install lattecode from the local checkout
	cargo install --path crates/lattecode

fmt: ## Format Rust source
	cargo fmt --all

fmt-check: ## Check Rust formatting
	cargo fmt --all -- --check

check: ## Run formatting, compilation, Clippy, and documentation checks
	./scripts/check.sh

lint: ## Run Clippy with warnings denied
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test: ## Run workspace tests and doctests
	./scripts/test.sh

doc: ## Build local API documentation
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

coverage: ## Run tests with the 90% line-coverage gate
	cargo llvm-cov clean --workspace
	cargo llvm-cov --workspace --all-features --all-targets --fail-under-lines 90

deny: ## Audit advisories, licenses, bans, and sources
	cargo deny check

ci: ## Reproduce the complete local CI gate
	./scripts/ci-local.sh

run: ## Run the headless CLI; pass arguments with ARGS='...'
	cargo run -p lattecode -- $(ARGS)

tui: ## Start the Ratatui interface
	cargo run -p lattecode -- tui

clean: ## Remove Cargo build and coverage output
	cargo llvm-cov clean --workspace 2>/dev/null || true
	cargo clean
	rm -rf coverage lcov.info
