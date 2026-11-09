#!/usr/bin/env bash
# Write docs/CLI.md from `--help` of buwiz-computer and every subcommand under it.
# Usage: scripts/gen-cli-docs.sh [output-file]   (default: docs/CLI.md)
# Deterministic: fixed terminal width, no color, and subcommands in the order clap lists them.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-${ROOT}/docs/CLI.md}"
cd "${ROOT}"

cargo build -q -p buwiz-computer
BIN="${CARGO_TARGET_DIR:-${ROOT}/target}/debug/buwiz-computer"
if [[ ! -x "${BIN}" ]]; then
  echo "cannot find ${BIN}" >&2
  exit 1
fi

export COLUMNS=100 NO_COLOR=1 CLICOLOR=0
# Help must not depend on the caller's environment.
unset BUWIZ_COMPUTER_EXEC_URL BUWIZ_COMPUTER_HOST_URL BUWIZ_COMPUTER_TOKEN

# Print the subcommand names listed in a help text, skipping clap's own `help`.
subcommands() {
  awk '
    /^Commands:/ { in_cmds = 1; next }
    in_cmds && /^$/ { in_cmds = 0 }
    in_cmds && /^  [a-z]/ { print $1 }
  ' | grep -vx 'help' || true
}

walk() {
  local path=("$@") help name
  help="$("${BIN}" "${path[@]:1}" --help | sed 's/[[:space:]]*$//')"
  {
    printf '## `%s`\n\n```text\n%s\n```\n\n' "${path[*]}" "${help}"
  } >>"${TMP}"
  while IFS= read -r name; do
    [[ -n "${name}" ]] && walk "${path[@]}" "${name}"
  done < <(printf '%s\n' "${help}" | subcommands)
}

TMP="$(mktemp)"
trap 'rm -f "${TMP}"' EXIT

cat >"${TMP}" <<'HEAD'
# CLI reference

`buwiz-computer` is the connect-only command line client. This page is generated from `--help` by `scripts/gen-cli-docs.sh`; CI fails when it is out of date (`scripts/check-cli-docs.sh`). Do not edit it by hand.

The three connection flags have environment forms (`BUWIZ_COMPUTER_EXEC_URL`, `BUWIZ_COMPUTER_HOST_URL`, `BUWIZ_COMPUTER_TOKEN`). Prefer the environment for the token: argv shows in `ps`.

HEAD

walk buwiz-computer

mkdir -p "$(dirname "${OUT}")"
cp "${TMP}" "${OUT}"
echo "wrote ${OUT}"
