#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

./scripts/check.sh
./scripts/test.sh
make coverage
cargo deny check

echo "All local CI checks passed."
