#!/usr/bin/env bash
# Same checks as smoke.sh, against locally built binaries (no Docker).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "${ROOT}"

TOKEN="${BOX_TOKEN:-local-smoke-box-token}"
export BOX_TOKEN="${TOKEN}"
export BOX_ID="${BOX_ID:-native-smoke}"
export WORKSPACE_ROOT="${WORKSPACE_ROOT:-${ROOT}/workspace-data}"
export BOX_EXEC_BIND="${BOX_EXEC_BIND:-127.0.0.1:1337}"
export BOX_HOST_BIND="${BOX_HOST_BIND:-127.0.0.1:1340}"
export BOX_EXEC_URL="${BOX_EXEC_URL:-http://127.0.0.1:1337}"
export BOX_DESKTOP="${BOX_DESKTOP:-0}"
export BOX_DESKTOP_REQUIRED="${BOX_DESKTOP_REQUIRED:-0}"
export BOX_CHROME="${BOX_CHROME:-0}"
export BOX_CUA="${BOX_CUA:-0}"
export RUST_LOG="${RUST_LOG:-warn}"
if [[ "${TOKEN}" == "dev-box-token" || ${#TOKEN} -lt 16 ]]; then
  export BOX_ALLOW_INSECURE_DEV=1
fi

mkdir -p "${WORKSPACE_ROOT}"

echo "==> cargo build"
cargo build -q -p box-exec -p box-host

./target/debug/box-exec &
EXEC_PID=$!
./target/debug/box-host &
HOST_PID=$!

cleanup() {
  kill -TERM "${EXEC_PID}" "${HOST_PID}" 2>/dev/null || true
  wait "${EXEC_PID}" 2>/dev/null || true
  wait "${HOST_PID}" 2>/dev/null || true
}
trap cleanup EXIT

ok=0
for _ in $(seq 1 40); do
  if curl -fsS http://127.0.0.1:1337/v1/health >/dev/null 2>&1 \
    && curl -fsS http://127.0.0.1:1340/v1/health >/dev/null 2>&1; then
    ok=1
    break
  fi
  sleep 0.25
done
if [[ "${ok}" != "1" ]]; then
  echo "native daemons never became healthy" >&2
  exit 1
fi

out="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["echo","ok"]}' \
  http://127.0.0.1:1337/v1/exec)"
echo "${out}"
echo "${out}" | grep -q '"exit_code":0'
echo "${out}" | grep -q '"exec_id"'
echo "${out}" | grep -q ok

code="$(curl -s -o /dev/null -w '%{http_code}' \
  -H "Content-Type: application/json" \
  -d '{"command":["true"]}' \
  http://127.0.0.1:1337/v1/exec)"
[[ "${code}" == "401" ]]

health="$(curl -fsS http://127.0.0.1:1337/v1/health)"
echo "${health}" | grep -q '"status":"ok"'
if echo "${health}" | grep -q '"service"'; then
  echo "public health must not fingerprint the service" >&2
  exit 1
fi

ready_unauth="$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:1340/v1/ready)"
if [[ "${ready_unauth}" != "401" ]]; then
  echo "expected 401 for /v1/ready without token, got ${ready_unauth}" >&2
  exit 1
fi

curl -fsS -H "Authorization: Bearer ${TOKEN}" http://127.0.0.1:1340/v1/ready \
  | grep -q '"exec_ready":true'

curl -fsS -H "Authorization: Bearer ${TOKEN}" http://127.0.0.1:1340/v1/info \
  | grep -q '"scope":"container-local"'

info="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" http://127.0.0.1:1340/v1/info)"
if command -v python3 >/dev/null 2>&1; then
  printf '%s' "${info}" | python3 -c '
import json, sys
c = json.load(sys.stdin)["capabilities"]
for name in ("exec", "files", "desktop", "chrome", "cua", "egress_tunnel"):
    assert "enabled" in c[name] and "ready" in c[name], name
'
fi

egress="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" http://127.0.0.1:1340/v1/egress)"
echo "${egress}"
echo "${egress}" | grep -q '"enabled":false'
echo "${egress}" | grep -q '"protocol":"box-egress-v1"'

rid="$(curl -sS -D - -o /dev/null -H "x-request-id: native-smoke" http://127.0.0.1:1337/v1/health)"
echo "${rid}" | grep -qi 'x-request-id: native-smoke'

busy="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" http://127.0.0.1:1337/v1/busy)"
echo "${busy}"
echo "${busy}" | grep -q '"max"'

stream="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -H "Accept: application/x-ndjson" \
  -d '{"command":["echo","stream"]}' \
  http://127.0.0.1:1337/v1/exec/stream)"
echo "${stream}"
echo "${stream}" | grep -q stream
echo "${stream}" | grep -q '"type":"exit"'

echo "NATIVE SMOKE OK"
