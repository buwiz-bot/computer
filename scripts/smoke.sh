#!/usr/bin/env bash
# Build the image, start the box, and prove health + authenticated exec.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "${ROOT}"

TOKEN="${BOX_TOKEN:-local-smoke-box-token}"
if [[ "${TOKEN}" == "dev-box-token" || ${#TOKEN} -lt 16 ]]; then
  echo "refusing insecure BOX_TOKEN for Compose (in-container binds are 0.0.0.0); using local-smoke-box-token" >&2
  TOKEN="local-smoke-box-token"
fi
export BOX_TOKEN="${TOKEN}"
export BOX_VIEWER_PASSWORD="${BOX_VIEWER_PASSWORD:-local-smoke-viewer-password}"
EXEC_URL="${EXEC_URL:-http://127.0.0.1:1337}"
HOST_URL="${HOST_URL:-http://127.0.0.1:1340}"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required for this smoke test" >&2
  echo "install Docker, or run: scripts/smoke-native.sh" >&2
  exit 1
fi

export COMPOSE_BAKE="${COMPOSE_BAKE:-false}"

COMPOSE=(docker compose)
if ! docker compose version >/dev/null 2>&1; then
  if command -v docker-compose >/dev/null 2>&1; then
    COMPOSE=(docker-compose)
  else
    echo "docker compose is required" >&2
    exit 1
  fi
fi
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    COMPOSE=(sudo -n docker compose)
  else
    echo "cannot talk to the Docker daemon (try adding this user to the docker group)" >&2
    exit 1
  fi
fi

mkdir -p workspace-data chrome-profile
chmod u+rwx workspace-data chrome-profile 2>/dev/null || true

# Compose mounts ./secrets/* instead of putting the values in the container
# environment, so they have to exist before anything is started.
bash "${ROOT}/scripts/write-secrets.sh"

echo "==> building and starting buwiz-computer"
"${COMPOSE[@]}" up --build -d

cleanup() {
  "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> waiting for health + ready"
ok=0
for _ in $(seq 1 90); do
  if curl -fsS "${EXEC_URL}/v1/health" >/dev/null 2>&1 \
    && curl -fsS "${HOST_URL}/v1/health" >/dev/null 2>&1 \
    && curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/ready" >/dev/null 2>&1; then
    ok=1
    break
  fi
  sleep 1
done
if [[ "${ok}" != "1" ]]; then
  echo "daemons never became healthy" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi

echo "==> GET /v1/health (exec, public, minimal)"
health="$(curl -fsS "${EXEC_URL}/v1/health")"
echo "${health}"
echo "${health}" | grep -q '"status":"ok"'
if echo "${health}" | grep -q '"service"'; then
  echo "public health must not fingerprint the service" >&2
  exit 1
fi

echo "==> GET /v1/ready requires bearer"
ready_unauth="$(curl -s -o /dev/null -w '%{http_code}' "${HOST_URL}/v1/ready")"
if [[ "${ready_unauth}" != "401" ]]; then
  echo "expected 401 for /v1/ready without token, got ${ready_unauth}" >&2
  exit 1
fi

echo "==> POST /v1/exec echo ok"
out="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["echo","ok"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${out}"
echo "${out}" | grep -q '"exit_code":0'
echo "${out}" | grep -q ok

echo "==> files put/get/mkdir/delete"
smoke_dir="smoke-dir-$$"
curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -X POST \
  -d "{\"path\":\"${smoke_dir}\",\"parents\":true}" \
  "${EXEC_URL}/v1/files/mkdir" | grep -q '"created":true'
curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -X PUT \
  -d "{\"path\":\"${smoke_dir}/smoke.txt\",\"content\":\"hello\"}" \
  "${EXEC_URL}/v1/files" >/dev/null
got="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  "${EXEC_URL}/v1/files?path=${smoke_dir}/smoke.txt")"
echo "${got}" | grep -q hello
curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -X DELETE \
  "${EXEC_URL}/v1/files?path=${smoke_dir}/smoke.txt" | grep -q '"deleted":true'
curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -X DELETE \
  "${EXEC_URL}/v1/files?path=${smoke_dir}&recursive=true" | grep -q '"deleted":true'

echo "==> exec does not leak BOX_TOKEN"
token_env="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","if [ -n \"${BOX_TOKEN+x}\" ]; then echo LEAKED; else echo STRIPPED; fi"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${token_env}"
echo "${token_env}" | grep -q STRIPPED
if echo "${token_env}" | grep -q LEAKED; then
  echo "BOX_TOKEN leaked into exec child" >&2
  exit 1
fi

echo "==> secrets are not in any /proc/<pid>/environ"
# The acceptance check for the file-delivered secrets: with BOX_TOKEN_FILE set,
# nothing in the box can recover the token or the viewer password by reading an
# environment block: not pid 1's, and not voxl's, which has to hold the password
# there and is execute-only so that it is not dumpable.
proc_environ="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","if grep -aqE \"(BOX_TOKEN|BOX_VIEWER_PASSWORD|VOXL_PASSWORD)=\" /proc/[0-9]*/environ 2>/dev/null; then echo PROC_LEAKED; else echo PROC_CLEAN; fi"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${proc_environ}"
if echo "${proc_environ}" | grep -q PROC_LEAKED; then
  echo "BOX_TOKEN or the viewer password is readable in a /proc/<pid>/environ" >&2
  exit 1
fi
echo "${proc_environ}" | grep -q PROC_CLEAN

echo "==> auth reject"
code="$(curl -s -o /dev/null -w '%{http_code}' \
  -H "Content-Type: application/json" \
  -d '{"command":["echo","x"]}' \
  "${EXEC_URL}/v1/exec")"
if [[ "${code}" != "401" ]]; then
  echo "expected 401 without token, got ${code}" >&2
  exit 1
fi

echo "==> GET /v1/info (host)"
info="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/info")"
echo "${info}"
echo "${info}" | grep -q '"box_id"'
echo "${info}" | grep -q '"scope":"container-local"'
if command -v python3 >/dev/null 2>&1; then
  printf '%s' "${info}" | python3 -c '
import json, sys
c = json.load(sys.stdin)["capabilities"]
for name in ("exec", "files", "desktop", "viewer", "chrome", "cua", "egress_tunnel"):
    assert "enabled" in c[name] and "ready" in c[name], name
assert c["exec"]["enabled"] and c["exec"]["ready"]
assert c["viewer"]["kind"] == "voxl" and c["viewer"]["port"] == 6901, c["viewer"]
print("capabilities objects ok")
'
fi

echo "==> GET /v1/desktop (host)"
desk="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/desktop")"
echo "${desk}"
echo "${desk}" | grep -q '"available":true'
echo "${desk}" | grep -q '"display":":1"'
echo "${desk}" | grep -q '"kind":"voxl"'
echo "${desk}" | grep -q '"port":6901'

echo "==> GET /v1/screens (host)"
for _ in $(seq 1 30); do
  screens="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/screens")"
  if echo "${screens}" | grep -q '"state":"running"'; then
    break
  fi
  sleep 1
done
echo "${screens}"
echo "${screens}" | grep -q '"display":":1"'
echo "${screens}" | grep -q '"viewer_port":6901'
echo "${screens}" | grep -q '"state":"running"'

echo "==> GET /v1/chrome (host)"
chrome="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/chrome")"
echo "${chrome}"
echo "${chrome}" | grep -q '"enabled":true'
echo "${chrome}" | grep -q '"cdp":"127.0.0.1:9222"'

echo "==> GET /v1/ready (host)"
ready="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/ready")"
echo "${ready}"
echo "${ready}" | grep -q '"exec_ready":true'
echo "${ready}" | grep -q '"desktop_ready":true'

echo "==> voxl viewer"
VIEWER_URL="${VIEWER_URL:-http://127.0.0.1:6901}"
curl -fsS "${VIEWER_URL}/healthz" | grep -q ok
# voxl's own login stays on: without the password there is no page.
viewer_unauth="$(curl -s -o /dev/null -w '%{http_code}' "${VIEWER_URL}/")"
if [[ "${viewer_unauth}" != "401" ]]; then
  echo "expected 401 from voxl without the password, got ${viewer_unauth}" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi
viewer_page="$(curl -s -o /dev/null -w '%{http_code}' -u ":${BOX_VIEWER_PASSWORD}" "${VIEWER_URL}/")"
if [[ "${viewer_page}" != "200" ]]; then
  echo "expected voxl's page 200 with the password, got ${viewer_page}" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi

echo "==> POST /v1/cua/wait"
waited="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"idle":200,"timeout":5000}' \
  "${EXEC_URL}/v1/cua/wait")"
echo "${waited}"
echo "${waited}" | grep -q '"seq"'

echo "==> POST /v1/cua/record/start + stop"
rec_id="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -X POST \
  "${EXEC_URL}/v1/cua/record/start" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')"
curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"x":200,"y":200}' \
  "${EXEC_URL}/v1/cua/move" >/dev/null
sleep 1
recording="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d "{\"id\":${rec_id}}" \
  "${EXEC_URL}/v1/cua/record/stop")"
echo "${recording}"
echo "${recording}" | grep -q '"path":"shared/recordings/'

echo "==> POST /v1/cua/screenshot"
shot="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -X POST \
  "${EXEC_URL}/v1/cua/screenshot")"
if ! printf '%s' "${shot}" | grep -q '"mime":"image/png"'; then
  echo "screenshot missing image/png mime" >&2
  echo "${shot:0:400}" >&2
  exit 1
fi
if command -v python3 >/dev/null 2>&1; then
  printf '%s' "${shot}" | python3 -c '
import json, sys, base64
d = json.load(sys.stdin)
b = base64.b64decode(d["png_base64"])
assert b[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
assert len(b) > 64, "screenshot too small"
assert d["width"] == 1280 and d["height"] == 800
print("png bytes", len(b))
'
else
  b64="$(printf '%s' "${shot}" | sed -n 's/.*"png_base64":"\([^"]*\)".*/\1/p')"
  if [[ -z "${b64}" || ${#b64} -lt 32 ]]; then
    echo "empty png_base64" >&2
    exit 1
  fi
  printf '%s' "${b64}" | base64 -d | head -c 8 | grep -q $'\x89PNG' || {
    echo "decoded screenshot was not a PNG" >&2
    exit 1
  }
fi

echo "==> POST /v1/cua/click (optional)"
click="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"x":640,"y":400,"button":1}' \
  "${EXEC_URL}/v1/cua/click")"
echo "${click}"
echo "${click}" | grep -q '"ok":true'

echo "==> POST /v1/cua/screenshot?format=png"
png_code="$(curl -sS -o /tmp/buwiz-computer-smoke.png -w '%{http_code}' \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Accept: image/png" \
  -X POST \
  "${EXEC_URL}/v1/cua/screenshot?format=png")"
if [[ "${png_code}" != "200" ]]; then
  echo "raw PNG screenshot expected 200, got ${png_code}" >&2
  exit 1
fi
head -c 8 /tmp/buwiz-computer-smoke.png | grep -q $'\x89PNG' || {
  echo "raw screenshot was not a PNG" >&2
  exit 1
}

echo "==> POST /v1/cua/move + double-click + drag"
move="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"x":100,"y":100}' \
  "${EXEC_URL}/v1/cua/move")"
echo "${move}" | grep -q '"ok":true'
dbl="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"x":100,"y":100}' \
  "${EXEC_URL}/v1/cua/double-click")"
echo "${dbl}" | grep -q '"ok":true'
drag="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"x1":100,"y1":100,"x2":120,"y2":120}' \
  "${EXEC_URL}/v1/cua/drag")"
echo "${drag}" | grep -q '"ok":true'

echo "==> POST /v1/cua/recipe (move + click in one call)"
recipe="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"name":"smoke","screenshot":"none","steps":[{"op":"move","x":120,"y":120},{"op":"click","x":120,"y":120,"button":1},{"op":"wait","ms":10}]}' \
  "${EXEC_URL}/v1/cua/recipe")"
echo "${recipe}"
echo "${recipe}" | grep -q '"ok":true'
echo "${recipe}" | grep -q '"ran":3'
# A recipe that did not ask to be observed must get the receipt it has always
# got. That is the compatibility promise, checked on the wire.
if echo "${recipe}" | grep -q '"observed"'; then
  echo "observe defaults to off; an unasked receipt must not carry observations" >&2
  exit 1
fi

echo "==> POST /v1/cua/recipe (observe: input)"
observed="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"name":"smoke-observe","screenshot":"none","observe":"input","steps":[{"op":"click","x":120,"y":120,"button":1},{"op":"type","text":"x"},{"op":"wait","ms":10}]}' \
  "${EXEC_URL}/v1/cua/recipe")"
echo "${observed}"
echo "${observed}" | grep -q '"ok":true'
echo "${observed}" | grep -q '"observed"'
echo "${observed}" | grep -q '"observe_ms"'
# The type step reports where the text was about to go, whatever the answer.
echo "${observed}" | grep -q '"focus"'
# A wait has nothing to look at and must not carry an empty block: absence of
# `observed` has to keep meaning "the box did not look".
if command -v python3 >/dev/null 2>&1; then
  printf '%s' "${observed}" | python3 -c '
import json, sys
steps = json.load(sys.stdin)["steps"]
seen = {s["op"]: ("observed" in s) for s in steps}
assert seen.get("click") is True, seen
assert seen.get("type") is True, seen
assert seen.get("wait") is False, seen
'
fi

echo "==> BOX_DESKTOP=0 still serves exec+host"
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
if [[ "${COMPOSE[0]}" == "sudo" ]]; then
  sudo -n env BOX_DESKTOP=0 BOX_DESKTOP_REQUIRED=0 docker compose up -d --force-recreate
else
  BOX_DESKTOP=0 BOX_DESKTOP_REQUIRED=0 "${COMPOSE[@]}" up -d --force-recreate
fi
ok=0
for _ in $(seq 1 90); do
  if curl -fsS "${EXEC_URL}/v1/health" >/dev/null 2>&1 \
    && curl -fsS "${HOST_URL}/v1/health" >/dev/null 2>&1; then
    ok=1
    break
  fi
  sleep 1
done
if [[ "${ok}" != "1" ]]; then
  echo "BOX_DESKTOP=0 daemons never became healthy" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi
off_info="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/info")"
echo "${off_info}"
if command -v python3 >/dev/null 2>&1; then
  printf '%s' "${off_info}" | python3 -c '
import json, sys
c = json.load(sys.stdin)["capabilities"]
assert c["desktop"]["enabled"] is False
assert c["chrome"]["enabled"] is False
assert c["cua"]["enabled"] is False or c["cua"]["ready"] is False
print("desktop-off capabilities ok")
'
else
  echo "${off_info}" | grep -q '"enabled":false'
fi
curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/ready" | grep -q '"exec_ready":true'
off_exec="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["echo","ok"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${off_exec}" | grep -q '"exit_code":0'

echo
echo "SMOKE OK"
