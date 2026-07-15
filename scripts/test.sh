#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

./scripts/test-inventory.sh
cargo test --workspace --lib --bins --all-features --locked
cargo test -p latte-core --test contracts --all-features --locked
cargo test -p latte-engine --test public_lifecycle --all-features --locked
cargo test -p latte-code --test architecture --test contract --test markdown_links --all-features --locked
cargo test -p latte-code --test e2e_portable --all-features --locked
cargo test -p latte-code --test e2e_unix --all-features --locked
cargo test --workspace --doc --all-features --locked
