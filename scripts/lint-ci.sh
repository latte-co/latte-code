#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if command -v actionlint >/dev/null 2>&1; then
  actionlint
elif command -v docker >/dev/null 2>&1; then
  docker run --rm -v "${PWD}:/repo" -w /repo rhysd/actionlint:1.7.12
else
  echo "error: actionlint or Docker is required" >&2
  exit 1
fi

shell_scripts=(scripts/*.sh)
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "${shell_scripts[@]}"
elif command -v docker >/dev/null 2>&1; then
  docker run --rm -v "${PWD}:/repo" -w /repo koalaman/shellcheck:v0.11.0 \
    "${shell_scripts[@]}"
else
  echo "error: ShellCheck or Docker is required" >&2
  exit 1
fi
