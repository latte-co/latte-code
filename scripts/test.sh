#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo test --workspace --all-targets --all-features
cargo test --workspace --doc --all-features
