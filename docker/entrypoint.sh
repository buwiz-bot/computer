#!/bin/bash
set -euo pipefail

# Every secret can arrive two ways: as a value (BOX_TOKEN) or as a path to a
# file holding it (BOX_TOKEN_FILE). The file form is the one to use. A value
# handed to the container through the environment is copied into the
# environment block of pid 1 at execve and stays readable there —
# /proc/1/environ — for every process in the box, for the life of the box.
# `unset` below does not undo that; it only changes what getenv sees.
read_secret() {
  local name="$1"
  local file_var="${name}_FILE"
  local path="${!file_var:-}"
  if [[ -n "${path}" ]]; then
    if [[ ! -r "${path}" ]]; then
      echo "${file_var} is set to ${path} but that file is not readable" >&2
      exit 1
    fi
    tr -d '\r\n' < "${path}"
    return 0
  fi
  printf '%s' "${!name:-}"
}

BOX_TOKEN_VALUE="$(read_secret BOX_TOKEN)"
if [[ -z "${BOX_TOKEN_VALUE}" ]]; then
  echo "BOX_TOKEN or BOX_TOKEN_FILE must be set" >&2
  exit 1
fi
BOX_HOST_TOKEN_VALUE="$(read_secret BOX_HOST_TOKEN)"
if [[ -z "${BOX_HOST_TOKEN_VALUE}" ]]; then
  BOX_HOST_TOKEN_VALUE="${BOX_TOKEN_VALUE}"
fi
BOX_VIEWER_PASSWORD_VALUE="$(read_secret BOX_VIEWER_PASSWORD)"
# Shell variables are not in the environ block, so these copies are not served
# by /proc/<pid>/environ the way the exported forms are.
unset BOX_TOKEN BOX_HOST_TOKEN BOX_VIEWER_PASSWORD || true

export WORKSPACE_ROOT="${WORKSPACE_ROOT:-/workspace}"
export BOX_DESKTOP="${BOX_DESKTOP:-1}"
export BOX_DISPLAY="${BOX_DISPLAY:-:1}"
# The shared screen, under a name a Bot's own screen never sets (it passes its own BOX_DISPLAY):
# box-active-tab and box-host tell the shared screen's report from a Bot's by it.
export BOX_SHARED_DISPLAY="${BOX_DISPLAY}"
export BOX_DISPLAY_GEOM="${BOX_DISPLAY_GEOM:-1280x800x24}"
export BOX_VIEWER_CTL="${BOX_VIEWER_CTL:-/tmp/box-viewer.ctl}"
export BOX_CHROME="${BOX_CHROME:-1}"
export BOX_CHROME_PROFILE="${BOX_CHROME_PROFILE:-${HOME:-/home/box}/chrome-profile}"
export BOX_CDP_PORT="${BOX_CDP_PORT:-9222}"
export BOX_CUA="${BOX_CUA:-1}"
# Listen on all interfaces *inside* the container netns so Compose port-map
# works. Host publish is 127.0.0.1 (see docker-compose.yml).
export BOX_EXEC_BIND="${BOX_EXEC_BIND:-0.0.0.0:1337}"
export BOX_HOST_BIND="${BOX_HOST_BIND:-0.0.0.0:1340}"
export BOX_EGRESS_TUNNEL="${BOX_EGRESS_TUNNEL:-0}"
export BOX_EGRESS_WS_BIND="${BOX_EGRESS_WS_BIND:-0.0.0.0:8790}"
export BOX_EGRESS_PROXY_BIND="${BOX_EGRESS_PROXY_BIND:-127.0.0.1:8791}"
export BOX_EGRESS_ADMIN_BIND="${BOX_EGRESS_ADMIN_BIND:-127.0.0.1:8792}"
export BOX_EGRESS_PROXY_PORT="${BOX_EGRESS_PROXY_PORT:-${BOX_EGRESS_PROXY_BIND##*:}}"

mkdir -p "${WORKSPACE_ROOT}/shared" "${BOX_CHROME_PROFILE}"

flag_on() {
  local v
  v="$(echo "${1:-}" | tr '[:upper:]' '[:lower:]')"
  [[ "${v}" != "0" && "${v}" != "false" && "${v}" != "off" && "${v}" != "no" ]]
}

stage_secret() {
  local path="$1" value="$2"
  (
    umask 077
    printf '%s' "${value}" >"${path}"
  )
  chmod 400 "${path}"
}

display_num() {
  echo "${BOX_DISPLAY}" | tr -d ':' | cut -d. -f1
}

geom_wh() {
  # BOX_DISPLAY_GEOM=WIDTHxHEIGHTxDEPTH → WIDTH HEIGHT
  local g w rest h
  g="${BOX_DISPLAY_GEOM}"
  w="${g%%x*}"
  rest="${g#*x}"
  h="${rest%%x*}"
  echo "${w} ${h}"
}

PIDS=()

record() {
  PIDS+=("$1")
}

shutdown() {
  echo "buwiz-computer shutting down"
  local pid
  rm -rf "${SECRET_DIR:-}" "${EGRESS_SECRET_DIR:-}" 2>/dev/null || true
  if [[ -f /tmp/box-chrome.pid ]]; then
    kill -TERM "$(cat /tmp/box-chrome.pid)" 2>/dev/null || true
  fi
  for pid in "${PIDS[@]}"; do
    kill -TERM "${pid}" 2>/dev/null || true
  done
  for pid in "${PIDS[@]}"; do
    wait "${pid}" 2>/dev/null || true
  done
}

trap shutdown SIGINT SIGTERM

start_desktop() {
  local dnum xvfb_screen
  dnum="$(display_num)"
  xvfb_screen="${BOX_DISPLAY_GEOM}"

  if [[ -z "${BOX_VIEWER_PASSWORD_VALUE}" ]]; then
    echo "BOX_VIEWER_PASSWORD or BOX_VIEWER_PASSWORD_FILE must be set when BOX_DESKTOP=1 (independent of BOX_TOKEN)" >&2
    exit 1
  fi

  # A container that was killed keeps the previous Xvfb's lock and socket in its layer, and
  # the next start reuses the same pid, so Xvfb reads its own stale lock as a live server and
  # refuses to start ("Server is already active for display 1") — forever, on every restart.
  # Nothing but this function ever starts an X server here, so at this point both are stale.
  rm -f "/tmp/.X${dnum}-lock" "/tmp/.X11-unix/X${dnum}"

  echo "starting Xvfb ${BOX_DISPLAY} ${xvfb_screen}"
  # XTEST is required for in-process CUA. DAMAGE tells voxl what changed, and -shmem lets it read
  # the framebuffer without a copy.
  Xvfb "${BOX_DISPLAY}" -screen 0 "${xvfb_screen}" -ac -shmem +extension GLX +extension XTEST +extension DAMAGE +render -noreset &
  record $!

  local n=0
  while [[ ${n} -lt 50 ]]; do
    if [[ -S "/tmp/.X11-unix/X${dnum}" ]]; then
      break
    fi
    sleep 0.1
    n=$((n + 1))
  done
  if [[ ! -S "/tmp/.X11-unix/X${dnum}" ]]; then
    echo "Xvfb did not create /tmp/.X11-unix/X${dnum}" >&2
    exit 1
  fi

  export DISPLAY="${BOX_DISPLAY}"

  echo "starting xfce4 on ${DISPLAY}"
  # xfce4-session brings up xfwm4, xfdesktop and the panel; they talk over
  # dbus. dbus-launch stays alive for the session, so the watchdog below
  # treats the desktop going away like any other required process.
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-box}"
  export XDG_CONFIG_HOME="${HOME:-/home/box}/.config"
  mkdir -p "${XDG_RUNTIME_DIR}"
  chmod 700 "${XDG_RUNTIME_DIR}"
  dbus-launch --exit-with-session xfce4-session >/tmp/xfce4-session.log 2>&1 &
  record $!

  start_viewer_keeper "${dnum}"
}

# One voxl per screen, on 6900+N inside the container (the host publishes loopback only). This
# subshell is the one thing in the box that keeps the password after startup: voxl takes it only
# from its argv, which every process can read, or its environment, so each voxl gets it there and
# the box's other processes do not. box-screen asks for a Bot's screen over the fifo, which names
# a display and nothing else. A voxl that dies is started again: the box's work does not depend on
# anyone watching it, so a viewer crash is not a reason to take the box down.
start_viewer_keeper() {
  local main="$1"
  rm -f "${BOX_VIEWER_CTL}"
  mkfifo -m 600 "${BOX_VIEWER_CTL}"
  (
    declare -A viewers=()
    # shellcheck disable=SC2329
    stop_viewers() {
      kill -TERM "${viewers[@]}" 2>/dev/null || true
      exit 0
    }
    trap stop_viewers TERM INT
    exec 8<>"${BOX_VIEWER_CTL}"
    start_viewer() {
      DISPLAY=":$1" VOXL_PASSWORD="${BOX_VIEWER_PASSWORD_VALUE}" voxl \
        --addr=0.0.0.0 \
        --port="$((6900 + $1))" \
        --title="Buwiz computer" \
        --audio-enabled=false \
        --microphone-enabled=false \
        --enable-resize=false \
        --file-transfers=none \
        >>"/tmp/voxl-$1.log" 2>&1 8>&- &
      viewers[$1]=$!
    }
    echo "starting voxl on 0.0.0.0:$((6900 + main)) (host publish should be loopback)"
    start_viewer "${main}"
    local verb n
    while true; do
      if read -r -t 0.5 verb n <&8 && [[ "${n}" =~ ^[0-9]{1,2}$ ]]; then
        case "${verb}" in
          up) [[ -n "${viewers[${n}]:-}" ]] || start_viewer "${n}" ;;
          down)
            if [[ -n "${viewers[${n}]:-}" ]]; then
              kill -TERM "${viewers[${n}]}" 2>/dev/null || true
              unset "viewers[${n}]"
            fi
            ;;
        esac
      fi
      for n in "${!viewers[@]}"; do
        if ! kill -0 "${viewers[${n}]}" 2>/dev/null; then
          echo "voxl on :${n} exited; starting it again" >&2
          start_viewer "${n}"
        fi
      done
    done
  ) &
  record $!
}

start_chrome() {
  local bin="" candidate w h
  for candidate in chromium chromium-browser google-chrome google-chrome-stable; do
    if command -v "${candidate}" >/dev/null 2>&1; then
      bin="${candidate}"
      break
    fi
  done
  if [[ -z "${bin}" ]]; then
    echo "chromium not found; capabilities.chrome will stay false" >&2
    return 0
  fi

  mkdir -p "${BOX_CHROME_PROFILE}"
  if [[ ! -w "${BOX_CHROME_PROFILE}" ]]; then
    echo "chrome profile ${BOX_CHROME_PROFILE} is not writable; using /tmp/box-chrome-profile" >&2
    export BOX_CHROME_PROFILE="/tmp/box-chrome-profile"
    mkdir -p "${BOX_CHROME_PROFILE}"
  fi
  # See box-refresh-extension: a profile kept from an older extension runs its old worker.
  BOX_SCREEN_WORKSPACE="${WORKSPACE_ROOT}/shared" /usr/local/bin/box-refresh-extension
  read -r w h < <(geom_wh)
  export DISPLAY="${BOX_DISPLAY}"

  echo "starting ${bin} on ${DISPLAY} profile=${BOX_CHROME_PROFILE} cdp=127.0.0.1:${BOX_CDP_PORT}"
  # uid 1000 cannot use Chrome's setuid sandbox in this image. --no-sandbox stays.
  # Do not pass --disable-setuid-sandbox: Chromium names that flag on the yellow
  # infobar. managed policy + --test-type hide the remaining --no-sandbox warning
  # so CUA screenshots are a usable desktop, not a banner.
  # Closing/crashing the browser must not take the box down — restart it.
  local proxy_args=()
  if flag_on "${BOX_EGRESS_TUNNEL}"; then
    echo "chromium proxy=http://127.0.0.1:${BOX_EGRESS_PROXY_PORT} (box-egress-tunnel)"
    proxy_args+=(
      --proxy-server="http://127.0.0.1:${BOX_EGRESS_PROXY_PORT}"
      --proxy-bypass-list="localhost;127.0.0.1;[::1];<-loopback>"
      # WebRTC ignores HTTP proxies: without this a page's STUN candidate
      # carries the VM's real address next to the laptop's, which is a worse
      # tell than either alone.
      --force-webrtc-ip-handling-policy=disable_non_proxied_udp
    )
  fi
  (
    while true; do
      # The profile is a bind mount, so it keeps Chromium's singleton lock across a kill and even
      # across a recreate of the container; a hostname/pid the new container happens to reuse
      # makes Chromium believe another instance owns it. This loop is the only thing that starts
      # Chromium on this profile, so before each start the lock is stale by definition.
      rm -f "${BOX_CHROME_PROFILE}/SingletonLock" "${BOX_CHROME_PROFILE}/SingletonSocket" \
        "${BOX_CHROME_PROFILE}/SingletonCookie"
      "${bin}" \
        --user-data-dir="${BOX_CHROME_PROFILE}" \
        --no-first-run \
        --no-default-browser-check \
        --disable-dev-shm-usage \
        --disable-gpu \
        --disable-software-rasterizer \
        --no-sandbox \
        --test-type \
        --hide-crash-restore-bubble \
        --disable-session-crashed-bubble \
        --disable-features=Translate \
        --load-extension=/opt/box/active-tab \
        --window-size="${w},${h}" \
        --window-position=0,0 \
        --remote-debugging-address=127.0.0.1 \
        --remote-debugging-port="${BOX_CDP_PORT}" \
        "${proxy_args[@]}" \
        about:blank \
        >/tmp/box-chrome.log 2>&1 &
      echo $! > /tmp/box-chrome.pid
      code=0
      wait $! || code=$?
      # A browser the person closed stays closed: exit 0 is a deliberate close from the live
      # screen, and a window that reappears two seconds later is not a box anyone can use. The
      # dock's launcher and `open_url` start it again when it is wanted. Anything else is a
      # crash, and a crash must not take the box down — restart it.
      if [[ "${code}" -eq 0 ]]; then
        echo "chromium closed; not restarting (the dock launcher or open_url starts it again)" >&2
        break
      fi
      echo "chromium exited with ${code}; restarting in 2s" >&2
      sleep 2
    done
  ) &
}

start_egress() {
  local bearer path n admin_port
  bearer="$(read_secret BOX_EGRESS_TUNNEL_BEARER)"
  if [[ -z "${bearer}" ]]; then
    echo "BOX_EGRESS_TUNNEL=1 requires BOX_EGRESS_TUNNEL_BEARER or BOX_EGRESS_TUNNEL_BEARER_FILE" >&2
    exit 1
  fi
  # Same story as BOX_TOKEN: a value in pid 1's environ is /proc-visible for
  # life. Unset after the copy; hand the daemon a staged file, never a value.
  unset BOX_EGRESS_TUNNEL_BEARER || true

  EGRESS_SECRET_DIR="$(mktemp -d /tmp/box-egress-secrets.XXXXXX)"
  chmod 700 "${EGRESS_SECRET_DIR}"
  path="${EGRESS_SECRET_DIR}/bearer"
  stage_secret "${path}" "${bearer}"
  bearer=""

  admin_port="${BOX_EGRESS_ADMIN_BIND##*:}"
  echo "starting box-egress-tunnel server ws=${BOX_EGRESS_WS_BIND} proxy=${BOX_EGRESS_PROXY_BIND} admin=127.0.0.1:${admin_port}"
  env BOX_EGRESS_WS_BIND="${BOX_EGRESS_WS_BIND}" \
    BOX_EGRESS_PROXY_BIND="${BOX_EGRESS_PROXY_BIND}" \
    BOX_EGRESS_ADMIN_BIND="${BOX_EGRESS_ADMIN_BIND}" \
    box-egress-tunnel server \
      --ws-bind "${BOX_EGRESS_WS_BIND}" \
      --proxy-bind "${BOX_EGRESS_PROXY_BIND}" \
      --admin-bind "${BOX_EGRESS_ADMIN_BIND}" \
      --bearer-file "${path}" \
      >/tmp/box-egress-tunnel.log 2>&1 &
  record $!

  n=0
  while [[ ${n} -lt 50 ]]; do
    if curl -fsS "http://127.0.0.1:${admin_port}/v1/status" >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
    n=$((n + 1))
  done
  if ! curl -fsS "http://127.0.0.1:${admin_port}/v1/status" >/dev/null 2>&1; then
    echo "box-egress-tunnel did not become ready (see /tmp/box-egress-tunnel.log)" >&2
    exit 1
  fi
  # Recorded for the dock's launcher: a Chromium started from a shell that did
  # not inherit BOX_EGRESS_TUNNEL (desktop icon, docker exec) used to come up
  # with no proxy at all and browse straight out of the VM.
  printf '%s\n' "${BOX_EGRESS_PROXY_PORT}" > /tmp/box-egress-proxy.port
}

echo "buwiz-computer starting box_id=${BOX_ID:-buwiz-computer} workspace=${WORKSPACE_ROOT} desktop=${BOX_DESKTOP} chrome=${BOX_CHROME} cua=${BOX_CUA} egress=${BOX_EGRESS_TUNNEL}"

if flag_on "${BOX_DESKTOP}"; then
  start_desktop
fi
if flag_on "${BOX_EGRESS_TUNNEL}"; then
  start_egress
fi
if flag_on "${BOX_DESKTOP}"; then
  if flag_on "${BOX_CHROME}"; then
    start_chrome
  fi
fi

# Hand the daemons a path, never a value. `env BOX_TOKEN=... box-exec` puts the
# token in that daemon's /proc/<pid>/environ at execve, where it stays for the
# life of the process and where the daemon cannot remove it. Each daemon reads
# its own copy and unlinks it, so the file exists for the few milliseconds
# between this write and daemon startup.
SECRET_DIR="$(mktemp -d /tmp/box-secrets.XXXXXX)"
chmod 700 "${SECRET_DIR}"

stage_secret "${SECRET_DIR}/exec.box_token" "${BOX_TOKEN_VALUE}"
stage_secret "${SECRET_DIR}/exec.host_token" "${BOX_HOST_TOKEN_VALUE}"
# box-exec reaches voxl for /wait and /record. box-host never needs the password, so it must not
# inherit a path to it; nor may box-exec be pointed at a file that was not staged.
exec_viewer=(-u BOX_VIEWER_PASSWORD_FILE)
if [[ -n "${BOX_VIEWER_PASSWORD_VALUE}" ]]; then
  stage_secret "${SECRET_DIR}/exec.viewer_password" "${BOX_VIEWER_PASSWORD_VALUE}"
  exec_viewer=("BOX_VIEWER_PASSWORD_FILE=${SECRET_DIR}/exec.viewer_password")
fi
stage_secret "${SECRET_DIR}/host.box_token" "${BOX_TOKEN_VALUE}"
stage_secret "${SECRET_DIR}/host.host_token" "${BOX_HOST_TOKEN_VALUE}"

env "${exec_viewer[@]}" BOX_TOKEN_FILE="${SECRET_DIR}/exec.box_token" \
  BOX_HOST_TOKEN_FILE="${SECRET_DIR}/exec.host_token" \
  box-exec &
record $!
env -u BOX_VIEWER_PASSWORD_FILE BOX_TOKEN_FILE="${SECRET_DIR}/host.box_token" \
  BOX_HOST_TOKEN_FILE="${SECRET_DIR}/host.host_token" \
  box-host &
record $!

# Net for the case where a daemon dies before it unlinks its own copy.
(
  sleep 30
  rm -rf "${SECRET_DIR}"
) >/dev/null 2>&1 &

BOX_TOKEN_VALUE=""
BOX_HOST_TOKEN_VALUE=""
BOX_VIEWER_PASSWORD_VALUE=""
unset BOX_TOKEN_VALUE BOX_HOST_TOKEN_VALUE BOX_VIEWER_PASSWORD_VALUE || true

while true; do
  for pid in "${PIDS[@]}"; do
    if ! kill -0 "${pid}" 2>/dev/null; then
      echo "a process exited (pid ${pid}); shutting down" >&2
      shutdown
      exit 1
    fi
  done
  sleep 1
done
