#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

./scripts/check.sh
./scripts/lint-ci.sh
./scripts/test.sh
make coverage
cargo deny --locked check

echo "All local CI checks passed."
