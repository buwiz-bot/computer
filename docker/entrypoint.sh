#!/bin/bash
set -euo pipefail

# The main screen's desktop: X, xfce, Chromium and the egress tunnel, as the box user. box-keeper
# starts this script and holds every secret, and this script must never read one: a shell cannot be
# made undumpable. box-keeper watches for the display itself and starts voxl and the daemons, and it
# decides BOX_DESKTOP and BOX_EGRESS_TUNNEL: it passes them here as 1 or 0. box-prepare, before it,
# made this screen's cookie (XAUTHORITY), its run folder (BOX_SCREEN_RUN) and the home.

export WORKSPACE_ROOT="${WORKSPACE_ROOT:-/workspace}"
export BOX_DESKTOP="${BOX_DESKTOP:-1}"
export BOX_DISPLAY="${BOX_DISPLAY:-:1}"
# The shared screen, under a name a Bot's own screen never sets (it passes its own BOX_DISPLAY):
# box-active-tab and box-host tell the shared screen's report from a Bot's by it.
export BOX_SHARED_DISPLAY="${BOX_DISPLAY}"
export BOX_DISPLAY_GEOM="${BOX_DISPLAY_GEOM:-1280x800x24}"
export BOX_CHROME="${BOX_CHROME:-1}"
export BOX_CHROME_PROFILE="${BOX_CHROME_PROFILE:-${HOME}/chrome-profile}"
export BOX_SCREEN_RUN="${BOX_SCREEN_RUN:?box-keeper names the run folder}"
# Everything on a desktop: see docker/shm-private.c.
export LD_PRELOAD=/usr/lib/box/libbox-shm-private.so
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

mkdir -p "${BOX_CHROME_PROFILE}"

flag_on() {
  local v
  v="$(echo "${1:-}" | tr '[:upper:]' '[:lower:]')"
  [[ "${v}" != "0" && "${v}" != "false" && "${v}" != "off" && "${v}" != "no" ]]
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
  if [[ -f "${BOX_SCREEN_RUN}/chrome.pid" ]]; then
    kill -TERM "$(cat "${BOX_SCREEN_RUN}/chrome.pid")" 2>/dev/null || true
  fi
  for pid in "${PIDS[@]}"; do
    kill -TERM "${pid}" 2>/dev/null || true
  done
  for pid in "${PIDS[@]}"; do
    wait "${pid}" 2>/dev/null || true
  done
}

trap 'shutdown; exit 0' SIGINT SIGTERM

start_desktop() {
  local dnum xvfb_screen
  dnum="$(display_num)"
  xvfb_screen="${BOX_DISPLAY_GEOM}"

  # box-prepare cleared /tmp: a killed container keeps the old Xvfb's lock and socket in its layer.

  echo "starting Xvfb ${BOX_DISPLAY} ${xvfb_screen}"
  # XTEST is required for in-process CUA. DAMAGE tells voxl what changed. Never -ac, and never
  # -shmem: Xvfb makes that framebuffer mode 0777, so any user could read the screen without asking
  # X anything. voxl reads through MIT-SHM into a segment of its own instead. The cookie file must
  # not be empty: an X server with an empty one lets any local client in.
  [[ -s "${XAUTHORITY}" ]] || { echo "no X cookie at ${XAUTHORITY}" >&2; exit 1; }
  Xvfb "${BOX_DISPLAY}" -screen 0 "${xvfb_screen}" -auth "${XAUTHORITY}" +extension GLX +extension XTEST +extension DAMAGE +render -noreset &
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
  export XDG_RUNTIME_DIR="${BOX_SCREEN_RUN}/runtime"
  export XDG_CONFIG_HOME="${HOME}/.config"
  mkdir -p "${XDG_RUNTIME_DIR}"
  chmod 700 "${XDG_RUNTIME_DIR}"
  dbus-launch --exit-with-session xfce4-session >"${BOX_SCREEN_RUN}/xfce4-session.log" 2>&1 &
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
    echo "chrome profile ${BOX_CHROME_PROFILE} is not writable" >&2
    exit 1
  fi
  # See box-refresh-extension: a profile kept from an older extension runs its old worker.
  BOX_SCREEN_WORKSPACE="${WORKSPACE_ROOT}/shared" /usr/local/bin/box-refresh-extension
  read -r w h < <(geom_wh)
  export DISPLAY="${BOX_DISPLAY}"

  echo "starting ${bin} on ${DISPLAY} profile=${BOX_CHROME_PROFILE}"
  # The box user cannot use Chrome's setuid sandbox in this image. --no-sandbox stays.
  # Do not pass --disable-setuid-sandbox: Chromium names that flag on the yellow
  # infobar. managed policy + --test-type hide the remaining --no-sandbox warning
  # so CUA screenshots are a usable desktop, not a banner.
  # No DevTools port: it has no login, and every user in the box shares this loopback.
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
        "${proxy_args[@]}" \
        about:blank \
        >"${BOX_SCREEN_RUN}/box-chrome.log" 2>&1 &
      echo $! > "${BOX_SCREEN_RUN}/chrome.pid"
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
  local n admin_port

  admin_port="${BOX_EGRESS_ADMIN_BIND##*:}"
  echo "starting box-egress-tunnel server ws=${BOX_EGRESS_WS_BIND} proxy=${BOX_EGRESS_PROXY_BIND} admin=127.0.0.1:${admin_port}"
  env BOX_EGRESS_WS_BIND="${BOX_EGRESS_WS_BIND}" \
    BOX_EGRESS_PROXY_BIND="${BOX_EGRESS_PROXY_BIND}" \
    BOX_EGRESS_ADMIN_BIND="${BOX_EGRESS_ADMIN_BIND}" \
    box-egress-tunnel server \
      --ws-bind "${BOX_EGRESS_WS_BIND}" \
      --proxy-bind "${BOX_EGRESS_PROXY_BIND}" \
      --admin-bind "${BOX_EGRESS_ADMIN_BIND}" \
      --bearer-file "${BOX_EGRESS_BEARER_STAGED}" \
      >"${BOX_SCREEN_RUN}/box-egress-tunnel.log" 2>&1 &
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
    echo "box-egress-tunnel did not become ready (see ${BOX_SCREEN_RUN}/box-egress-tunnel.log)" >&2
    exit 1
  fi
  # Recorded for the dock's launcher: a Chromium started from a shell that did
  # not inherit BOX_EGRESS_TUNNEL (desktop icon, docker exec) used to come up
  # with no proxy at all and browse straight out of the VM.
  printf '%s\n' "${BOX_EGRESS_PROXY_PORT}" > /run/box/egress/port
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

while true; do
  for pid in "${PIDS[@]:-}"; do
    [[ -n "${pid}" ]] || continue
    if ! kill -0 "${pid}" 2>/dev/null; then
      echo "a process exited (pid ${pid}); shutting down" >&2
      shutdown
      exit 1
    fi
  done
  sleep 1
done
