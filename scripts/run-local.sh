#!/usr/bin/env bash
# Run box-exec + box-host on the host (no container).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "${ROOT}"

if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi

if [[ -z "${BOX_TOKEN:-}" ]]; then
  echo "BOX_TOKEN is not set. Export it or add it to .env (the value is not printed)." >&2
  exit 1
fi

export BOX_TOKEN
export BOX_ID="${BOX_ID:-local-dev}"
export WORKSPACE_ROOT="${WORKSPACE_ROOT:-${ROOT}/workspace-data}"
export BOX_EXEC_BIND="${BOX_EXEC_BIND:-127.0.0.1:1337}"
export BOX_HOST_BIND="${BOX_HOST_BIND:-127.0.0.1:1340}"
export BOX_EXEC_URL="${BOX_EXEC_URL:-http://127.0.0.1:1337}"
export BOX_DESKTOP="${BOX_DESKTOP:-0}"
export BOX_DESKTOP_REQUIRED="${BOX_DESKTOP_REQUIRED:-0}"
export BOX_CHROME="${BOX_CHROME:-0}"
export BOX_CUA="${BOX_CUA:-0}"
export RUST_LOG="${RUST_LOG:-info}"

if [[ "${BOX_TOKEN}" == "dev-box-token" || ${#BOX_TOKEN} -lt 16 ]]; then
  export BOX_ALLOW_INSECURE_DEV="${BOX_ALLOW_INSECURE_DEV:-1}"
fi

mkdir -p "${WORKSPACE_ROOT}"
echo "BOX_TOKEN is set (value not printed)  workspace=${WORKSPACE_ROOT}"
echo "exec  ${BOX_EXEC_BIND}"
echo "host  ${BOX_HOST_BIND}"
if [[ "${BOX_ALLOW_INSECURE_DEV:-}" == "1" ]]; then
  echo "BOX_ALLOW_INSECURE_DEV=1 (loopback binds only)"
fi

cargo build -p box-exec -p box-host

./target/debug/box-exec &
EXEC_PID=$!
./target/debug/box-host &
HOST_PID=$!

shutdown() {
  kill -TERM "${EXEC_PID}" "${HOST_PID}" 2>/dev/null || true
  wait "${EXEC_PID}" 2>/dev/null || true
  wait "${HOST_PID}" 2>/dev/null || true
}
trap shutdown INT TERM
wait -n "${EXEC_PID}" "${HOST_PID}" || true
shutdown
