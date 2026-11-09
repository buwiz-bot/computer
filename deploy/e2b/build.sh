#!/usr/bin/env bash
# Builds the Buwiz Bot computer template in YOUR E2B team, from the box image. One command:
#   E2B_API_KEY=<your key> bash deploy/e2b/build.sh
# Needs Python 3.10 or newer and network access. The E2B SDK goes in a venv under
# ~/.cache/buwiz-e2b (BUWIZ_E2B_VENV overrides it). Running it again rebuilds the same template.
# The key is read from the environment by the SDK and is never printed.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
SDK_VERSION="2.10.2"
VENV="${BUWIZ_E2B_VENV:-${HOME}/.cache/buwiz-e2b/venv}"

if [[ -z "${E2B_API_KEY:-}" ]]; then
  echo "set E2B_API_KEY to your E2B team's API key (dashboard: API Keys). Nothing was built." >&2
  exit 2
fi
command -v python3 >/dev/null 2>&1 || { echo "needs python3 (3.10 or newer)" >&2; exit 1; }
if ! python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)'; then
  echo "needs Python 3.10 or newer; this is $(python3 -V 2>&1)" >&2
  exit 1
fi
for f in e2b.Dockerfile template.py; do
  [[ -f "${ROOT}/${f}" ]] || { echo "missing ${ROOT}/${f}" >&2; exit 1; }
done

if ! "${VENV}/bin/python" -c "import importlib.metadata as m, sys; sys.exit(0 if m.version('e2b') == '${SDK_VERSION}' else 1)" >/dev/null 2>&1; then
  echo "installing the E2B SDK ${SDK_VERSION} into ${VENV}"
  python3 -m venv "${VENV}"
  "${VENV}/bin/pip" install --quiet --disable-pip-version-check "e2b==${SDK_VERSION}"
fi

"${VENV}/bin/python" "${ROOT}/template.py"

echo
echo "Paste the alias into Buwiz Bot as the setting BUWIZ_E2B_TEMPLATE (the id works too)."
echo "Set E2B_API_KEY in the same place, so the broker can make sandboxes from it."
