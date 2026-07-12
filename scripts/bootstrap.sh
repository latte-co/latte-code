#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is not installed; install Rust with rustup first" >&2
  exit 1
fi

if command -v rustup >/dev/null 2>&1; then
  rustup component add rustfmt clippy llvm-tools-preview
else
  echo "warning: rustup is unavailable; skipping component installation" >&2
fi

install_cargo_tool() {
  local command_name="$1"
  local crate_name="$2"

  if cargo "$command_name" --version >/dev/null 2>&1; then
    echo "ok: cargo-$command_name is already installed"
  else
    cargo install "$crate_name" --locked
  fi
}

install_cargo_tool llvm-cov cargo-llvm-cov
install_cargo_tool deny cargo-deny

echo "Local Rust development tools are ready."
