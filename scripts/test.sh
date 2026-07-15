#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

./scripts/test-inventory.sh
cargo test --workspace --lib --bins --all-features
cargo test -p latte-core --test contracts --all-features
cargo test -p latte-engine --test public_lifecycle --all-features
cargo test -p latte-code --test architecture --test contract --test markdown_links --all-features
cargo test -p latte-code --test e2e --all-features
cargo test --workspace --doc --all-features
