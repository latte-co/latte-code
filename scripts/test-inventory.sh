#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

expected="$(printf '%s\n' \
  'crates/latte-code/tests/architecture.rs contract' \
  'crates/latte-code/tests/contract.rs contract' \
  'crates/latte-code/tests/e2e.rs e2e' \
  'crates/latte-code/tests/markdown_links.rs contract' \
  'crates/latte-core/tests/contracts.rs contract' \
  'crates/latte-engine/tests/public_lifecycle.rs contract')"

actual="$({
  for tests_dir in crates/*/tests; do
    if [[ -d "${tests_dir}" ]]; then
      find "${tests_dir}" -maxdepth 1 -type f -name '*.rs' -print
    fi
  done
} | sort | awk '{
  layer = ($0 == "crates/latte-code/tests/e2e.rs") ? "e2e" : "contract"
  print $0, layer
}')"

if [[ "${actual}" != "${expected}" ]]; then
  echo "Integration-test inventory is out of date." >&2
  diff -u <(printf '%s\n' "${expected}") <(printf '%s\n' "${actual}") || true
  exit 1
fi

if rg -n '#\[ignore' crates --glob '*.rs'; then
  echo "Required Rust tests must not use #[ignore]." >&2
  exit 1
fi

echo "Test inventory passed: 5 contract targets, 1 E2E target, no ignored tests."
