#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

./scripts/check.sh
./scripts/test.sh
cargo llvm-cov clean --workspace
cargo llvm-cov --workspace --all-features --all-targets --fail-under-lines 90
cargo deny check

echo "All local CI checks passed."
