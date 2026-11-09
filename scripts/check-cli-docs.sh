#!/usr/bin/env bash
# Fail if docs/CLI.md is not what scripts/gen-cli-docs.sh writes now.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp)"
trap 'rm -f "${TMP}"' EXIT

bash "${ROOT}/scripts/gen-cli-docs.sh" "${TMP}" >/dev/null
if ! diff -u "${ROOT}/docs/CLI.md" "${TMP}"; then
  echo "docs/CLI.md is out of date: run bash scripts/gen-cli-docs.sh" >&2
  exit 1
fi
echo "CLI docs OK"
