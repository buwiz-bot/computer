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
# this user's own: on a Linux host workspace-data is the box's (uid 1000), not the runner's.
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
