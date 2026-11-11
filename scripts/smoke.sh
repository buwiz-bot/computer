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

# A project of its own, one per checkout: its home volume is created clean here and deleted at the
# end, and a box you run yourself with `docker compose up` in this folder keeps its own.
# SMOKE_PROJECT may name another, but only a buwiz-smoke-… one: smoke deletes the project's home
# volume (`down -v`), so it never takes a name a real box could have.
PROJECT="${SMOKE_PROJECT:-buwiz-smoke-$(printf '%s' "${ROOT}" | cksum | cut -d' ' -f1)}"
if [[ ! "${PROJECT}" =~ ^buwiz-smoke-[a-z0-9][a-z0-9_-]*$ ]]; then
  echo "SMOKE_PROJECT must start with buwiz-smoke- (lowercase letters, digits, - and _ after it): smoke deletes its project's home volume, so it won't run as ${PROJECT}" >&2
  exit 1
fi

COMPOSE=(docker compose -p "${PROJECT}")
if ! docker compose version >/dev/null 2>&1; then
  if command -v docker-compose >/dev/null 2>&1; then
    COMPOSE=(docker-compose -p "${PROJECT}")
  else
    echo "docker compose is required" >&2
    exit 1
  fi
fi
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    COMPOSE=(sudo -n docker compose -p "${PROJECT}")
  else
    echo "cannot talk to the Docker daemon (try adding this user to the docker group)" >&2
    exit 1
  fi
fi

mkdir -p workspace-data
chmod u+rwx workspace-data 2>/dev/null || true
# Compose mounts ./secrets/* instead of putting the values in the container
# environment, so they have to exist before anything is started.
bash "${ROOT}/scripts/write-secrets.sh"

# A home a previous run left would make this one pass or fail on what that run saved.
"${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true

echo "==> building and starting buwiz-computer (project ${PROJECT})"
"${COMPOSE[@]}" up --build -d

# -v: this project's home volume goes with it. Only this project's; the workspace is a bind mount.
cleanup() {
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
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

echo "==> an exec child cannot read a secret from any process's memory"
# The probe gets the secrets on stdin, so only it holds them, and it skips itself. It first finds
# a secret in a child of its own, so a clean result means the scan works, not that it saw nothing.
memory_out="$(python3 "${ROOT}/scripts/run-memory-probe.py" "${EXEC_URL}" "${TOKEN}" "${BOX_VIEWER_PASSWORD}")"
echo "${memory_out}"
for held in "keeper denied" "exec denied" "host denied" "voxl denied"; do
  echo "${memory_out}" | grep -q "${held}"
done
echo "${memory_out}" | grep -q "control found"
echo "${memory_out}" | grep -q "leaks none"

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
echo "${chrome}" | grep -q '"running":true'
if echo "${chrome}" | grep -q '"cdp"'; then
  echo "the browser must have no DevTools port" >&2
  exit 1
fi

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
  printf '%s' "${b64}" | base64 -d | head -c 8 | LC_ALL=C grep -aq $'\x89PNG' || {
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
head -c 8 /tmp/buwiz-computer-smoke.png | LC_ALL=C grep -aq $'\x89PNG' || {
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

echo "==> POST /v1/cua/tree + /v1/cua/element (AT-SPI)"
# The panel and Chromium register on the screen's accessibility bus a moment after the desktop is
# up: wait for both, then check the panel's Files button and Chromium's toolbar are in the tree.
tree=""
for _ in $(seq 1 30); do
  tree="$(curl -fsS \
    -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"scope":"desktop","max_nodes":800}' \
    "${EXEC_URL}/v1/cua/tree" || true)"
  if printf '%s' "${tree}" | grep -q '"name":"Files"' \
    && printf '%s' "${tree}" | grep -q '"name":"Reload"'; then
    break
  fi
  sleep 1
done
files_ref=""
if command -v python3 >/dev/null 2>&1; then
  files_ref="$(printf '%s' "${tree}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["scope"] == "desktop" and d["display"] == ":1", d
def walk(nodes):
    for n in nodes:
        yield n
        yield from walk(n.get("children", []))
nodes = list(walk(d["nodes"]))
assert len(nodes) == d["count"], "count is the nodes sent"
apps = {n["name"] for n in d["nodes"]}
assert "xfce4-panel" in apps and "Chromium" in apps, apps
files = [n for n in nodes if n["role"] == "button" and n["name"] == "Files"]
assert files, "the panel has no Files button"
b = files[0]["bounds"]
assert 0 <= b["x"] < 1280 and 0 <= b["y"] < 800 and b["width"] > 0, b
assert any(n["role"] == "button" and n["name"] == "Reload" for n in nodes), "no Chromium toolbar"
assert len({n["ref"] for n in nodes}) == len(nodes), "refs repeat"
print(files[0]["ref"])
')"
else
  printf '%s' "${tree}" | grep -q '"role":"button","name":"Files"'
  files_ref="$(printf '%s' "${tree}" | sed -n 's/.*"ref":"\([0-9]*:[0-9]*\)","role":"button","name":"Files".*/\1/p')"
fi
if [[ -z "${files_ref}" ]]; then
  echo "accessibility tree has no Files button: ${tree:0:600}" >&2
  exit 1
fi
echo "Files button ${files_ref}"
curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{}' "${EXEC_URL}/v1/cua/tree" | grep -q '"scope":"focused_window"'
# The tree above is no longer the latest, so its refs are stale.
stale_code="$(curl -sS -o /dev/null -w '%{http_code}' \
  -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d "{\"ref\":\"${files_ref}\",\"action\":\"focus\"}" "${EXEC_URL}/v1/cua/element")"
if [[ "${stale_code}" != "409" ]]; then
  echo "expected 409 stale_ref for an older tree's ref, got ${stale_code}" >&2
  exit 1
fi
a11y_exec="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","if [ -r /run/box/x/1/run ] || [ -w /run/box/x/1/run/a11y-bus ]; then echo A11Y_REACHED; else echo A11Y_WALLED; fi"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${a11y_exec}"
echo "${a11y_exec}" | grep -q A11Y_WALLED

echo "==> the browser's DevTools pipe: on a pipe, no port, out of a command's reach"
# As a command sees it: Chromium is on a pipe and has no port, the pipe's socket is behind a
# folder it can't enter, and nothing listens on TCP but the box's own ports (127.0.0.11 is Docker's
# own resolver, in every container). Chromium's lines are picked by name, so this command's own
# words aren't read as Chromium's, and printed whole: ps cuts them at 80 columns otherwise.
# shellcheck disable=SC2016 # expanded by the box's shell, not this one
pipe_exec="$(curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","args=$(ps -ww -eo comm=,args= | awk '\''$1 == \"chromium\"'\''); case \"$args\" in *remote-debugging-pipe*) echo ON_PIPE;; esac; case \"$args\" in *remote-debugging-port*) echo HAS_PORT;; esac; if [ -e /run/box/x/1/run/cdp.sock ]; then echo PIPE_REACHED; else echo PIPE_WALLED; fi; for f in /proc/net/tcp /proc/net/tcp6; do awk '\''$4 == \"0A\" && $2 !~ /^0B00007F:/ { split($2, a, \":\"); print a[2] }'\'' \"$f\"; done | sort -u | while read -r hex; do echo \"LISTEN $((0x$hex))\"; done"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${pipe_exec}"
echo "${pipe_exec}" | grep -q ON_PIPE
echo "${pipe_exec}" | grep -q PIPE_WALLED
if echo "${pipe_exec}" | grep -q HAS_PORT; then
  echo "a Chromium in the box has a DevTools port" >&2
  exit 1
fi
for port in $(printf '%s' "${pipe_exec}" | grep -o 'LISTEN [0-9]*' | cut -d' ' -f2); do
  case "${port}" in
    1337 | 1340 | 690[1-5] | 879[0-2]) ;;
    *)
      echo "something listens on TCP ${port}" >&2
      exit 1
      ;;
  esac
done
# As the desktop user, who owns Chromium: its fds 3 and 4 are one socket, the launcher is
# undumpable, and the socket it offers is in the screen's run folder.
# shellcheck disable=SC2016 # expanded by the box's shell, not this one
pipe_desk="$("${COMPOSE[@]}" exec -T -u box box sh -c '
  launcher=$(pgrep -u box -o -x box-chrome-pipe)
  chrome=$(pgrep -u box -o -P "${launcher:-0}" -x chromium)
  [ -n "${chrome}" ] && [ -n "${launcher}" ] || { echo NO_PIPE_CHROME; exit 0; }
  three=$(readlink "/proc/${chrome}/fd/3"); four=$(readlink "/proc/${chrome}/fd/4")
  case "${three}" in socket:*) [ "${three}" = "${four}" ] && echo FDS_ONE_SOCKET;; esac
  ls "/proc/${launcher}/fd" >/dev/null 2>&1 || echo LAUNCHER_UNDUMPABLE
  [ -S /run/box/x/1/run/cdp.sock ] && echo SOCKET_OFFERED
  true')"
echo "${pipe_desk}"
echo "${pipe_desk}" | grep -q FDS_ONE_SOCKET
echo "${pipe_desk}" | grep -q LAUNCHER_UNDUMPABLE

echo "==> POST /v1/browser/* on a page served inside the box"
# bpost <route> <json>: the answer in ${body}, its status in ${code}. The answer goes to a file of
# this user's own: on a Linux host box-prepare gives workspace-data to the box's users (agent, 1500,
# and root at the top), so the runner can't write there.
bpost() {
  local answer
  answer="$(mktemp "${TMPDIR:-/tmp}/browser-answer.XXXXXX")"
  code="$(curl -sS -o "${answer}" -w '%{http_code}' \
    -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
    -d "$2" "${EXEC_URL}/v1/browser/$1")"
  body="$(cat "${answer}")"
  rm -f "${answer}"
}
# ref_of <label> <role>: the @eN of the element with that label and role in the snap in ${body}.
# A <label for> is an element of its own, with its field's name, so the role tells them apart.
ref_of() {
  printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
hits = [e["ref"] for e in d["elements"] if e["label"] == sys.argv[1] and e["role"] == sys.argv[2]]
assert hits, (sys.argv[1:], [(e["label"], e["role"]) for e in d["elements"]])
print(hits[0])
' "$1" "$2"
}
# The page: a command serves it, as any command may on a port of its own.
fixture="$(python3 -c 'import json,sys; print(json.dumps({"path":"shared/browser-shop.pl","content":open(sys.argv[1]).read()}))' \
  "${ROOT}/scripts/fixtures/browser-shop.pl")"
curl -fsS -X PUT -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d "${fixture}" "${EXEC_URL}/v1/files" >/dev/null
curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","nohup perl /workspace/shared/browser-shop.pl >/dev/null 2>&1 & for i in $(seq 50); do curl -fsS -o /dev/null http://127.0.0.1:6910/ && exit 0; sleep 0.1; done; exit 1"]}' \
  "${EXEC_URL}/v1/exec" | grep -q '"exit_code":0'
allow='["127.0.0.1"]'

bpost navigate "{\"url\":\"http://127.0.0.1:6910/\",\"allow_domains\":${allow}}"
echo "navigate ${code} ${body}"
[[ "${code}" == "200" ]]
# box-exec took the pipe: the launcher no longer offers it.
"${COMPOSE[@]}" exec -T -u box box sh -c '[ ! -e /run/box/x/1/run/cdp.sock ]'
echo "the launcher's socket is gone once box-exec holds the pipe"
bpost wait '{"text":"Fixture shop","load":"load","timeout_ms":15000}'
echo "wait ${code} ${body}"
printf '%s' "${body}" | grep -q '"seen":true'

bpost snap '{}'
[[ "${code}" == "200" ]]
printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
labels = [e["label"] for e in d["elements"]]
print("snap", d["snapshot"], d["url"], labels)
assert d["url"] == "http://127.0.0.1:6910/", d["url"]
assert {"Email", "Password", "Search", "Pay $18"} <= set(labels), labels
# No password text: no Password element has a value, whatever ultra-instinct can say of its type.
assert all("value" not in e for e in d["elements"] if e["label"] == "Password"), d["elements"]
print("roles", sorted({(e["label"], e["role"]) for e in d["elements"]}))
'
if printf '%s' "${body}" | grep -q 'hunter2-smoke-pw'; then
  echo "a password field's value came back" >&2
  exit 1
fi
snapshot="$(printf '%s' "${body}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["snapshot"])')"
email="$(ref_of Email text_field)"
search="$(ref_of Search button)"
pay="$(ref_of 'Pay $18' button)"

bpost act "{\"ref\":\"${email}\",\"snapshot\":${snapshot},\"action\":\"type\",\"text\":\"kid@example.com\",\"allow_domains\":${allow}}"
echo "type ${code} ${body}"
[[ "${code}" == "200" ]]
bpost act "{\"ref\":\"${search}\",\"action\":\"click\",\"allow_domains\":${allow}}"
echo "click ${code} ${body}"
[[ "${code}" == "200" ]]
bpost wait '{"text":"searched kid@example.com","timeout_ms":10000}'
echo "wait ${code} ${body}"
printf '%s' "${body}" | grep -q '"seen":true'

# With values asked for, the email field's typed text comes back; the password's and the card's never do.
bpost snap '{"values":true}'
echo "values with no list ${code}"
[[ "${code}" == "400" ]]
printf '%s' "${body}" | grep -q 'values needs allow_domains'
bpost snap "{\"values\":true,\"allow_domains\":${allow}}"
[[ "${code}" == "200" ]]
printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
by = {e["label"]: e for e in d["elements"] if e["role"] == "text_field"}
assert by["Email"].get("value") == "kid@example.com", by["Email"]
for secret in ("Password", "Card"):
    e = by[secret]
    assert e.get("sensitive") and not ({"value", "value_truncated", "checked", "expanded"} & e.keys()), e
assert "value=\"kid@example.com\"" in d["text"], d["text"]
print("values: the email field shows its text, the password and card fields show none")
'
if printf '%s' "${body}" | grep -q 'hunter2-smoke-pw'; then
  echo "a password field's value came back with values" >&2
  exit 1
fi

# ultra-instinct marks the password field sensitive, and no field's text comes back.
bpost snap '{}'
[[ "${code}" == "200" ]]
printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
email = [e for e in d["elements"] if e["label"] == "Email" and e["role"] == "text_field"]
pw = [e for e in d["elements"] if e["label"] == "Password" and e["role"] == "text_field"]
assert email and not any(e.get("sensitive") for e in email), email
assert pw and all(e.get("sensitive") and "value" not in e for e in pw), pw
print("the password field is sensitive, the email field is not, and neither has text")
'
if printf '%s' "${body}" | grep -q 'hunter2-smoke-pw'; then
  echo "a password field's value came back" >&2
  exit 1
fi
pay="$(ref_of 'Pay $18' button)"

# A Pay click needs a person's confirm, and goes through with the token.
bpost act "{\"ref\":\"${pay}\",\"action\":\"click\",\"allow_domains\":${allow}}"
echo "pay ${code} ${body}"
[[ "${code}" == "409" ]]
printf '%s' "${body}" | grep -q '"code":"confirm_required"'
printf '%s' "${body}" | grep -q '"label":"Pay \$18"'
token="$(printf '%s' "${body}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["error"]["details"]["confirm"])')"
bpost act "{\"ref\":\"${pay}\",\"action\":\"click\",\"confirm\":\"${token}\",\"allow_domains\":${allow}}"
echo "pay confirmed ${code} ${body}"
[[ "${code}" == "200" ]]
bpost wait '{"text":"paid in full","timeout_ms":10000}'
printf '%s' "${body}" | grep -q '"seen":true'
bpost act "{\"ref\":\"${pay}\",\"action\":\"click\",\"confirm\":\"${token}\",\"allow_domains\":${allow}}"
[[ "${code}" == "409" ]]
echo "a spent token confirms nothing: ${code}"

# A key confirms by the form it sends: Enter in a field outside any form presses nothing, and
# goes through; Enter in the checkout's field would send it, so it needs Place order's confirm.
bpost snap '{}'
[[ "${code}" == "200" ]]
email="$(ref_of Email text_field)"
card="$(ref_of Card text_field)"
bpost act "{\"ref\":\"${email}\",\"action\":\"key\",\"text\":\"Enter\",\"allow_domains\":${allow}}"
echo "enter outside a form ${code} ${body}"
[[ "${code}" == "200" ]]
bpost act "{\"ref\":\"${card}\",\"action\":\"key\",\"text\":\"Enter\",\"allow_domains\":${allow}}"
echo "enter in the checkout ${code} ${body}"
[[ "${code}" == "409" ]]
printf '%s' "${body}" | grep -q '"code":"confirm_required"'
printf '%s' "${body}" | grep -q '"label":"Place order"'
token="$(printf '%s' "${body}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["error"]["details"]["confirm"])')"
bpost act "{\"ref\":\"${card}\",\"action\":\"key\",\"text\":\"Enter\",\"confirm\":\"${token}\",\"allow_domains\":${allow}}"
echo "enter confirmed ${code} ${body}"
[[ "${code}" == "200" ]]
bpost wait '{"text":"order placed","timeout_ms":10000}'
printf '%s' "${body}" | grep -q '"seen":true'

# Off the allow-list: refused before the browser goes anywhere.
bpost navigate "{\"url\":\"https://example.org/\",\"allow_domains\":${allow}}"
echo "off-list ${code} ${body}"
[[ "${code}" == "403" ]]
printf '%s' "${body}" | grep -q '"code":"domain_not_allowed"'

bpost shot '{}'
[[ "${code}" == "200" ]]
printf '%s' "${body}" | python3 -c '
import base64, json, sys
d = json.load(sys.stdin)
png = base64.b64decode(d["data"])
assert d["format"] == "png" and png[:8] == b"\x89PNG\r\n\x1a\n" and len(png) == d["bytes"], d["bytes"]
print("shot", d["width"], "x", d["height"], d["bytes"], "bytes")
'

bpost state '{"op":"save","name":"smoke"}'
echo "state save ${code} ${body}"
[[ "${code}" == "200" ]]
"${COMPOSE[@]}" exec -T box sh -c 'stat -c "%a %U" /home/box/browser-state/main /home/box/browser-state/main/smoke.json' | tee /dev/stderr | tr '\n' ' ' | grep -q '^700 boxsvc 600 boxsvc'
state_exec="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","if cat /home/box/browser-state/main/smoke.json >/dev/null 2>&1; then echo STATE_READ; else echo STATE_WALLED; fi"]}' \
  "${EXEC_URL}/v1/exec")"
echo "${state_exec}" | grep -q STATE_WALLED
bpost state '{"op":"load","name":"smoke"}'
echo "state load ${code} ${body}"
[[ "${code}" == "200" ]]

bpost tabs '{"op":"open","url":"about:blank"}'
echo "tabs open ${code} ${body}"
[[ "${code}" == "200" ]]
new_tab="$(printf '%s' "${body}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["tab_id"])')"
bpost tabs '{"op":"list"}'
echo "tabs list ${code}"
printf '%s' "${body}" | grep -q "${new_tab}"
bpost tabs "{\"op\":\"close\",\"tab_id\":\"${new_tab}\"}"
[[ "${code}" == "200" ]]
# A goal run: Instinct's loop on the current tab, under the call's allow-list and confirm rules.
bpost run "{\"goal\":\"Click Search\",\"max_steps\":4,\"allow_domains\":${allow}}"
echo "run ${code} ${body}"
[[ "${code}" == "200" ]]
printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["outcome"] == "done", d
assert any(s["target"] == "Search" for s in d["steps"]), d["steps"]
print("run done in", len(d["steps"]), "step(s)")
'
# A run that would press Pay stops for a person's confirm, and goes on with the token.
bpost run "{\"goal\":\"Click Pay \$18\",\"max_steps\":4,\"allow_domains\":${allow}}"
echo "run pay ${code} ${body}"
[[ "${code}" == "200" ]]
run_token="$(printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["outcome"] == "blocked" and d.get("code") == "needs_confirm" and d["confirm_label"] == "Pay $18" and d["confirm"], d
print(d["confirm"])
')"
bpost run "{\"goal\":\"Click Pay \$18\",\"max_steps\":4,\"confirm\":\"${run_token}\",\"allow_domains\":${allow}}"
echo "run pay confirmed ${code} ${body}"
[[ "${code}" == "200" ]]
printf '%s' "${body}" | grep -q '"outcome":"done"'

echo "==> a relaunched Chromium offers a fresh pipe, which box-exec takes on its next call"
"${COMPOSE[@]}" exec -T -u box box sh -c 'kill -KILL "$(pgrep -u box -o -P "$(pgrep -u box -o -x box-chrome-pipe)" -x chromium)"'
relaunched=0
for _ in $(seq 1 40); do
  bpost snap '{}'
  if [[ "${code}" == "200" ]]; then
    relaunched=1
    break
  fi
  sleep 0.5
done
if [[ "${relaunched}" != "1" ]]; then
  echo "box-exec never took the relaunched Chromium's pipe: ${code} ${body}" >&2
  exit 1
fi
echo "snap after the relaunch ${code}"
"${COMPOSE[@]}" exec -T -u box box sh -c '[ ! -e /run/box/x/1/run/cdp.sock ]'

echo "==> a launcher starts Chromium again only when box-exec let go of a pipe that still worked"
# No route makes box-exec drop a live pipe, so a stand-in takes its place: a second, headless
# Chromium on a launcher of its own, in a folder like a run folder, and, as boxsvc, a few lines of
# Python that take its pipe as box-exec does and let go as box-exec would. box-exec's own requests
# are too small to fill the socket within a smoke's time, so the stand-in does that part:
#   let-go:   Chromium stopped (SIGSTOP), a request written until the socket is full and cut off
#             there, as ultra-instinct's deadline cuts one and then holds the pipe closed; then L,
#             its answer, the pipe closed, and Chromium let run again: it reads half a frame and
#             the end, and quits 0: 75, started again ("counted")
#   close:    the lease and the pipe closed with no L (box-exec's shutdown): 0, left closed
#   person:   Browser.close, the lease held (a person closing it): 0
#   inflight: Browser.close, the pipe read to its end as a call in flight would, then L at once,
#             as the call's failure sends it, while Chromium is still quitting: 0, not counted
#             because "Chromium had shut its pipe"
# Chromium quits with 0 in every case: the status is the launcher's call. Then box-chromium, on
# that launcher, is sent a TERM: it reaches the launcher, and nothing restarts.
lease_out="$("${COMPOSE[@]}" exec -T box sh <<'LEASE'
set -u
dir=/tmp/pipe-smoke
rm -rf "$dir" /tmp/pipe-smoke-home
install -d -o box -g boxsvc -m 0750 "$dir"
install -d -o box -g box -m 0700 /tmp/pipe-smoke-home
PY='
import socket, sys
path, how = sys.argv[1], sys.argv[2]
lease = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
lease.connect(path)
_, fds, _, _ = socket.recv_fds(lease, 1, 1)
pipe = socket.socket(fileno=fds[0])
def let_go():
    try:
        lease.settimeout(1)
        lease.sendall(b"L")
        lease.recv(1)
    except OSError:
        pass
if how == "let-go":
    pipe.setblocking(False)
    head = b"{\"id\":1,\"method\":\"Input.insertText\",\"params\":{\"text\":\""
    try:
        pipe.send(head)
        while True:
            pipe.send(b"x" * 65536)
    except BlockingIOError:
        pass
    let_go()
    lease.close()
    pipe.close()
    sys.exit()
pipe.sendall(b"{\"id\":1,\"method\":\"Browser.getVersion\"}\0")
got = b""
while b"\0" not in got:
    more = pipe.recv(65536)
    if not more:
        sys.exit("the pipe closed before Chromium answered")
    got += more
if how == "close":
    lease.close()
    pipe.close()
elif how == "person":
    pipe.sendall(b"{\"id\":2,\"method\":\"Browser.close\"}\0")
    lease.recv(1)
elif how == "inflight":
    pipe.sendall(b"{\"id\":2,\"method\":\"Browser.close\"}\0")
    while pipe.recv(65536):
        pass
    let_go()
    lease.close()
    pipe.close()
else:
    lease.recv(1)
'
launch() {
  rm -rf /tmp/pipe-smoke-home/profile
  setpriv --reuid=1000 --regid=1000 --clear-groups env HOME=/tmp/pipe-smoke-home "$@" \
    >/tmp/pipe-smoke-home/launcher.log 2>&1 &
  launcher=$!
  for _ in $(seq 100); do [ -S "$dir/cdp.sock" ] && break; sleep 0.1; done
}
for how in let-go close person inflight; do
  launch /usr/local/bin/box-chrome-pipe "$dir/cdp.sock" -- chromium --headless=new --no-sandbox \
    --disable-gpu --user-data-dir=/tmp/pipe-smoke-home/profile about:blank
  chromium=""
  if [ "$how" = let-go ]; then
    # this launcher's own Chromium, never the screen's
    for _ in $(seq 50); do
      chromium=$(pgrep -P "$launcher" -x chromium) && break
      sleep 0.1
    done
    sleep 1
    kill -STOP "$chromium"
  fi
  setpriv --reuid=990 --regid=990 --clear-groups python3 -c "$PY" "$dir/cdp.sock" "$how"
  [ -z "$chromium" ] || kill -CONT "$chromium"
  code=0
  wait "$launcher" || code=$?
  said=$(grep -o 'let go[^:]*: .*' /tmp/pipe-smoke-home/launcher.log | head -1)
  echo "LEASE ${how} ${code} ${said}"
done
# box-chromium on a pipe: its TERM reaches the launcher and Chromium, and nothing starts again
launch env BOX_BROWSER=1 BOX_SCREEN_RUN="$dir" BOX_EGRESS_TUNNEL=0 \
  BOX_CHROME_PROFILE=/tmp/pipe-smoke-home/profile /usr/local/bin/box-chromium --headless=new
setpriv --reuid=990 --regid=990 --clear-groups python3 -c "$PY" "$dir/cdp.sock" hold &
holder=$!
sleep 1
kill -TERM "$launcher"
code=0
wait "$launcher" || code=$?
sleep 3
left=$(pgrep -u box -f "pipe-smoke-home/profile" | wc -l)
offered=no
[ -e "$dir/cdp.sock" ] && offered=yes
kill "$holder" 2>/dev/null
echo "TRAP code=${code} left=${left} offered=${offered}"
rm -rf "$dir" /tmp/pipe-smoke-home
LEASE
)"
echo "${lease_out}"
echo "${lease_out}" | grep -qx 'LEASE let-go 75 let go while Chromium ran: counted'
echo "${lease_out}" | grep -qx 'LEASE close 0 *'
echo "${lease_out}" | grep -qx 'LEASE person 0 *'
echo "${lease_out}" | grep -qx 'LEASE inflight 0 let go, not counted: Chromium had shut its pipe'
echo "${lease_out}" | grep -q '^TRAP code=[0-9]* left=0 offered=no$'

echo "==> the home volume outlives the container: logins, profiles, a Bot's slot and user"
# spost <json>: POST /v1/screen, the answer in ${screen}.
spost() {
  screen="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
    -d "$1" "${EXEC_URL}/v1/screen")"
}
bot_uid() {
  curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
    -d '{"command":["id","-u"],"bot":"smoke"}' "${EXEC_URL}/v1/exec" \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["exit_code"]==0, d; print(d["stdout"].strip())'
}
spost '{"action":"up","bot":"smoke"}'
echo "screen up ${screen}"
keep_slot="$(printf '%s' "${screen}" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert "error" not in d, d; print(d["slot"])')"
keep_uid="$(bot_uid)"
spost '{"action":"down","bot":"smoke"}'
"${COMPOSE[@]}" exec -T -u box box sh -c 'echo kept >/home/box/desktops/main/chrome-profile/.smoke-kept'
"${COMPOSE[@]}" exec -T box sh -c 'test -d /home/box/desktops/smoke/chrome-profile && test -f /home/box/.box/slots/smoke'
echo "before: Bot smoke on slot ${keep_slot} as uid ${keep_uid}"
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
"${COMPOSE[@]}" up -d --force-recreate
ok=0
for _ in $(seq 1 90); do
  if curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/ready" 2>/dev/null | grep -q '"desktop_ready":true' \
    && curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/chrome" 2>/dev/null | grep -q '"running":true'; then
    ok=1
    break
  fi
  sleep 1
done
if [[ "${ok}" != "1" ]]; then
  echo "the recreated box never became ready" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi
loaded=0
for _ in $(seq 1 20); do
  bpost state '{"op":"load","name":"smoke"}'
  if [[ "${code}" == "200" ]]; then
    loaded=1
    break
  fi
  sleep 0.5
done
echo "state load after recreate ${code} ${body}"
[[ "${loaded}" == "1" ]]
"${COMPOSE[@]}" exec -T box sh -c 'cat /home/box/desktops/main/chrome-profile/.smoke-kept; test -d /home/box/desktops/smoke/chrome-profile && echo BOT_PROFILE_KEPT' \
  | tee /dev/stderr | tr '\n' ' ' | grep -q '^kept BOT_PROFILE_KEPT'
spost '{"action":"list"}'
echo "screen list after recreate ${screen}"
printf '%s' "${screen}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
hits = [s for s in d["screens"] if s["bot"] == "smoke"]
assert hits and hits[0]["slot"] == int(sys.argv[1]), (sys.argv[1], d)
print("slot kept", hits[0]["slot"])
' "${keep_slot}"
after_uid="$(bot_uid)"
[[ "${after_uid}" == "${keep_uid}" ]]
echo "uid kept ${after_uid}"

echo "==> BOX_CHROME=0 (as Buwiz Bot's broker makes a computer): a Bot's own screen still has /v1/browser/*, and a login is filled through it"
# The broker's fill: open the page on the Bot's screen, read the front tab, snap with the card's
# site as allow_domains, type each field by ref (the password only into a sensitive one), and press
# Enter in the password field: a "Sign in" form asks no confirm. The password is a fake marker that
# must never come back.
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
if [[ "${COMPOSE[0]}" == "sudo" ]]; then
  sudo -n env BOX_CHROME=0 docker compose -p "${PROJECT}" up -d --force-recreate
else
  BOX_CHROME=0 "${COMPOSE[@]}" up -d --force-recreate
fi
ok=0
for _ in $(seq 1 90); do
  if curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/ready" 2>/dev/null | grep -q '"desktop_ready":true'; then
    ok=1
    break
  fi
  sleep 1
done
if [[ "${ok}" != "1" ]]; then
  echo "the BOX_CHROME=0 box never became ready" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi
curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/info" | python3 -c '
import json, sys
c = json.load(sys.stdin)["capabilities"]
assert c["chrome"]["enabled"] is False, c["chrome"]
assert c["browser"]["enabled"] is True, c.get("browser")
print("BOX_CHROME=0: chrome off, browser on")
'
# The main screen stays closed to the browser layer with BOX_CHROME=0, as before: disabled, even
# for a Chromium a person might open there from the dock.
bpost snap '{}'
echo "main screen snap ${code} ${body}"
[[ "${code}" == "503" ]]
printf '%s' "${body}" | grep -q '"code":"browser_disabled"'
curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","nohup perl /workspace/shared/browser-shop.pl >/dev/null 2>&1 & for i in $(seq 50); do curl -fsS -o /dev/null http://127.0.0.1:6910/login && exit 0; sleep 0.1; done; exit 1"]}' \
  "${EXEC_URL}/v1/exec" | grep -q '"exit_code":0'
spost '{"action":"up","bot":"smoke"}'
echo "screen up ${screen}"
bot_display="$(printf '%s' "${screen}" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert "error" not in d, d; print(d["display"])')"
spost '{"action":"open","bot":"smoke","url":"http://127.0.0.1:6910/login"}'
echo "screen open ${screen}"
on="?display=${bot_display}"
seen=0
for _ in $(seq 1 40); do
  bpost "wait${on}" '{"text":"Fixture sign-in","load":"load","timeout_ms":5000}'
  if [[ "${code}" == "200" ]] && printf '%s' "${body}" | grep -q '"seen":true'; then
    seen=1
    break
  fi
  sleep 0.5
done
echo "bot screen wait ${code} ${body}"
[[ "${seen}" == "1" ]]
front=""
for _ in $(seq 1 40); do
  front="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/chrome/active-tab${on}")"
  if printf '%s' "${front}" | grep -q '"url":"http://127.0.0.1:6910/login"'; then
    break
  fi
  sleep 0.25
done
echo "front tab ${front}"
printf '%s' "${front}" | grep -q '"url":"http://127.0.0.1:6910/login"'
bpost "snap${on}" "{\"allow_domains\":${allow}}"
echo "bot screen snap ${code}"
[[ "${code}" == "200" ]]
printf '%s' "${body}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["url"] == "http://127.0.0.1:6910/login", d["url"]
by = {e["label"]: e for e in d["elements"] if e["role"] == "text_field"}
assert not by["Email"].get("sensitive"), by["Email"]
assert by["Password"].get("sensitive") is True, by["Password"]
print("sign-in form: the email field is not sensitive, the password field is")
'
snapshot="$(printf '%s' "${body}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["snapshot"])')"
email="$(ref_of Email text_field)"
bpost "act${on}" "{\"ref\":\"${email}\",\"snapshot\":${snapshot},\"action\":\"type\",\"text\":\"kid@example.com\",\"allow_domains\":${allow}}"
echo "type email ${code}"
[[ "${code}" == "200" ]]
# A fresh snap before each field, as the broker does: a ref is good for its snapshot only.
bpost "snap${on}" "{\"allow_domains\":${allow}}"
[[ "${code}" == "200" ]]
snapshot="$(printf '%s' "${body}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["snapshot"])')"
pw="$(ref_of Password text_field)"
bpost "act${on}" "{\"ref\":\"${pw}\",\"snapshot\":${snapshot},\"action\":\"type\",\"text\":\"fake-smoke-login-pw\",\"allow_domains\":${allow}}"
echo "type password ${code}"
[[ "${code}" == "200" ]]
if printf '%s' "${body}" | grep -q 'fake-smoke-login-pw'; then
  echo "a typed password came back" >&2
  exit 1
fi
bpost "act${on}" "{\"ref\":\"${pw}\",\"action\":\"key\",\"text\":\"Enter\",\"allow_domains\":${allow}}"
echo "enter in the sign-in form ${code} ${body}"
[[ "${code}" == "200" ]]
bpost "wait${on}" '{"text":"signed in as kid@example.com","timeout_ms":10000}'
echo "signed in ${code} ${body}"
printf '%s' "${body}" | grep -q '"seen":true'
bpost "snap${on}" "{\"values\":true,\"allow_domains\":${allow}}"
[[ "${code}" == "200" ]]
if printf '%s' "${body}" | grep -q 'fake-smoke-login-pw'; then
  echo "a typed password came back in a snap" >&2
  exit 1
fi
# Off the card's site: nothing typed.
bpost "act${on}" "{\"ref\":\"${pw}\",\"action\":\"type\",\"text\":\"x\",\"allow_domains\":[\"example.org\"]}"
echo "off the card's site ${code}"
[[ "${code}" == "403" ]]
printf '%s' "${body}" | grep -q '"code":"domain_not_allowed"'
bpost "state${on}" '{"op":"save","name":"signin"}'
echo "bot screen state save ${code} ${body}"
[[ "${code}" == "200" ]]
bpost "state${on}" '{"op":"load","name":"signin"}'
echo "bot screen state load ${code} ${body}"
[[ "${code}" == "200" ]]
spost '{"action":"down","bot":"smoke"}'

echo "==> the browser's proxy: the computer's rules, dialled from the box, then through the tunnel"
# Every destination is the fixture shop on the box's loopback: the box's proxy and the client may
# dial loopback and any port here (BOX_EGRESS_ALLOW_PRIVATE, BOX_EGRESS_RELAY_PORTS) and nowhere
# else, so nothing leaves for the internet. The bearer and the starting rules come in a folder of
# this run's own, mounted read-only; the bearer is 32 random bytes, written by python, never in
# argv or the environment.
egress_dir="$(mktemp -d "${TMPDIR:-/tmp}/buwiz-smoke-egress.XXXXXX")"
chmod 0755 "${egress_dir}"
python3 -c 'import secrets, sys; open(sys.argv[1], "w").write(secrets.token_urlsafe(32))' "${egress_dir}/bearer"
chmod 0644 "${egress_dir}/bearer"
cat >"${egress_dir}/rules.json" <<'RULES'
{"v":1,"version":1,"exit":"direct","mode":"listed","allow":["localhost"],"block":["blocked.test"]}
RULES
cat >"${egress_dir}/compose.yml" <<YAML
services:
  box:
    volumes:
      - ${egress_dir}:/run/smoke-egress:ro
YAML
# On top of whatever files this run's Compose reads (COMPOSE_FILE, else docker-compose.yml).
egress_env=("COMPOSE_FILE=${COMPOSE_FILE:-docker-compose.yml}:${egress_dir}/compose.yml"
  BOX_CHROME=0 BOX_EGRESS_PROXY=1 BOX_EGRESS_TUNNEL=1
  BOX_EGRESS_TUNNEL_BEARER_FILE=/run/smoke-egress/bearer
  BOX_EGRESS_RULES_FILE=/run/smoke-egress/rules.json
  BOX_EGRESS_ALLOW_PRIVATE=1 "BOX_EGRESS_RELAY_PORTS=*")
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
if [[ "${COMPOSE[0]}" == "sudo" ]]; then
  sudo -n env "${egress_env[@]}" docker compose -p "${PROJECT}" up -d --force-recreate
else
  env "${egress_env[@]}" "${COMPOSE[@]}" up -d --force-recreate
fi
ok=0
for _ in $(seq 1 90); do
  if curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/ready" 2>/dev/null | grep -q '"desktop_ready":true'; then
    ok=1
    break
  fi
  sleep 1
done
if [[ "${ok}" != "1" ]]; then
  echo "the box with the proxy never became ready" >&2
  "${COMPOSE[@]}" logs || true
  exit 1
fi
egress_status() {
  curl -fsS -H "Authorization: Bearer ${TOKEN}" "${HOST_URL}/v1/egress"
}
egress_status | python3 -c '
import json, sys
s = json.load(sys.stdin)
assert s["proxy_enabled"] and s["proxy_up"], s
assert s["rules"] == {"version": 1, "exit": "direct", "mode": "listed", "allow": 1, "block": 1}, s["rules"]
assert s["can_browse"] is True, s
assert "localhost" not in json.dumps(s), "a status names no site"
print("the staged rules are in force before the box answers ready")
'
curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","nohup perl /workspace/shared/browser-shop.pl >/dev/null 2>&1 & for i in $(seq 50); do curl -fsS -o /dev/null http://127.0.0.1:6910/ && exit 0; sleep 0.1; done; exit 1"]}' \
  "${EXEC_URL}/v1/exec" | grep -q '"exit_code":0'
# through <host>: a CONNECT to <host>:6910 through the box's proxy, as a command; prints the
# CONNECT's status and, when it passed, the page's.
through() {
  curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
    -d "{\"command\":[\"sh\",\"-c\",\"curl -s -o /dev/null -w '%{http_connect} %{http_code}' --noproxy '' -p -x http://127.0.0.1:8791 http://$1:6910/ || true\"]}" \
    "${EXEC_URL}/v1/exec" | python3 -c 'import json,sys; print(json.load(sys.stdin)["stdout"].strip())'
}
got="$(through localhost)"; echo "allowed, direct: ${got}"; [[ "${got}" == "200 200" ]]
got="$(through blocked.test)"; echo "blocked: ${got}"; [[ "${got}" == 403* ]]
got="$(through elsewhere.test)"; echo "not listed: ${got}"; [[ "${got}" == 403* ]]
plain="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d "{\"command\":[\"sh\",\"-c\",\"curl -s -o /dev/null -w '%{http_code}' --noproxy '' -x http://127.0.0.1:8791 http://localhost:6910/ || true\"]}" \
  "${EXEC_URL}/v1/exec" | python3 -c 'import json,sys; print(json.load(sys.stdin)["stdout"].strip())')"
echo "plain http through the proxy: ${plain}"; [[ "${plain}" == "405" ]]
# A Bot's screen's browser goes through the proxy too: a plain http:// page gets the proxy's answer.
spost '{"action":"up","bot":"smoke"}'
bot_display="$(printf '%s' "${screen}" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert "error" not in d, d; print(d["display"])')"
spost '{"action":"open","bot":"smoke","url":"http://blocked.test:6910/"}'
seen=0
for _ in $(seq 1 40); do
  bpost "wait?display=${bot_display}" '{"text":"loads https:// pages only","load":"load","timeout_ms":5000}'
  if [[ "${code}" == "200" ]] && printf '%s' "${body}" | grep -q '"seen":true'; then
    seen=1
    break
  fi
  sleep 0.5
done
echo "bot screen through the proxy ${code} ${body}"
[[ "${seen}" == "1" ]]
spost '{"action":"down","bot":"smoke"}'
# rules_put <json>: PUT /v1/egress/rules, its status in ${code}. The body goes in a file.
rules_put() {
  local file
  file="$(mktemp "${TMPDIR:-/tmp}/rules.XXXXXX")"
  printf '%s' "$1" >"${file}"
  code="$(curl -sS -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer ${TOKEN}" \
    -H "Content-Type: application/json" --data-binary "@${file}" "${HOST_URL}/v1/egress/rules")"
  rm -f "${file}"
}
rules_put '{"v":1,"version":2,"exit":"direct","mode":"all","block":["localhost"]}'
echo "rules changed at runtime: ${code}"; [[ "${code}" == "200" ]]
got="$(through localhost)"; echo "now blocked: ${got}"; [[ "${got}" == 403* ]]
rules_put '{"v":1,"version":3,"exit":"direct","mode":"all","block":["*"]}'
echo "a block of * is refused: ${code}"; [[ "${code}" == "422" ]]
got="$(through localhost)"; echo "and changed nothing: ${got}"; [[ "${got}" == 403* ]]
# The tunnel as the exit, with no client attached: refused with 503, never dialled from the box.
rules_put '{"v":1,"version":4,"exit":"tunnel","mode":"listed","allow":["localhost"],"block":["blocked.test"]}'
echo "tunnel on: ${code}"; [[ "${code}" == "200" ]]
got="$(through localhost)"; echo "tunnel, no client: ${got}"; [[ "${got}" == 503* ]]
egress_status | python3 -c '
import json, sys
s = json.load(sys.stdin)
assert s["can_browse"] is False and s["ready"] is False, s
assert s["counts"]["no_client"] >= 1, s["counts"]
'
# A local client attached: the box's own client binary, run as a command, on the published side of
# 8790 inside the box. It reads the bearer from the read-only folder.
curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","BOX_EGRESS_ALLOW_PRIVATE=1 BOX_EGRESS_RELAY_PORTS=\"*\" nohup box-egress-tunnel client --url ws://127.0.0.1:8790 --bearer-file /run/smoke-egress/bearer --reconnect >/dev/null 2>&1 & echo $! >/workspace/shared/egress-client.pid"]}' \
  "${EXEC_URL}/v1/exec" | grep -q '"exit_code":0'
ok=0
for _ in $(seq 1 50); do
  if egress_status | grep -q '"client_attached":true'; then
    ok=1
    break
  fi
  sleep 0.2
done
[[ "${ok}" == "1" ]]
got="$(through localhost)"; echo "tunnel, client attached: ${got}"; [[ "${got}" == "200 200" ]]
got="$(through blocked.test)"; echo "tunnel, blocked: ${got}"; [[ "${got}" == 403* ]]
egress_status | python3 -c '
import json, sys
s = json.load(sys.stdin)
assert s["can_browse"] is True and s["counts"]["tunneled"] >= 1, s
print("relayed through the client:", s["counts"]["tunneled"])
'
curl -fsS -H "Authorization: Bearer ${TOKEN}" -H "Content-Type: application/json" \
  -d '{"command":["sh","-c","kill $(cat /workspace/shared/egress-client.pid)"]}' \
  "${EXEC_URL}/v1/exec" | grep -q '"exit_code":0'
ok=0
for _ in $(seq 1 50); do
  if egress_status | grep -q '"client_attached":false'; then
    ok=1
    break
  fi
  sleep 0.2
done
[[ "${ok}" == "1" ]]
got="$(through localhost)"; echo "client gone: ${got}"; [[ "${got}" == 503* ]]
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
rm -rf "${egress_dir}"

echo "==> BOX_DESKTOP=0 still serves exec+host"
"${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
if [[ "${COMPOSE[0]}" == "sudo" ]]; then
  sudo -n env BOX_DESKTOP=0 BOX_DESKTOP_REQUIRED=0 docker compose -p "${PROJECT}" up -d --force-recreate
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
# and the second recreate, with no screen at all, still finds the login on disk
"${COMPOSE[@]}" exec -T box test -f /home/box/browser-state/main/smoke.json

echo
echo "SMOKE OK"
