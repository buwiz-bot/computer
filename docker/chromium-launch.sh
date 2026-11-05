#!/bin/bash
# NOT installed in the guest image.
#
# The product Chromium is started by docker/entrypoint.sh (respawn loop, CDP on
# loopback). This script is a leftover dock-toggle launcher used by older demo
# recipes. box-exec / recipes must not call `box-chromium`. Do not COPY this
# file into the image unless you are deliberately replacing the entrypoint
# model.
#
# Launch Chromium as a normal desktop app (no kiosk, no respawn).
#
# Chromium itself adds --disable-setuid-sandbox whenever --no-sandbox is set,
# then the yellow infobar names that flag. We never pass
# --disable-setuid-sandbox. We hide the warning with managed policy +
# --test-type, and we exec the ELF so Debian's /usr/bin/chromium wrapper
# cannot re-inject flags from /etc/chromium.d.
#
# --test-type makes chrome://newtab/ fail ("incorrect profile type") so the
# last-tab close looks like a blank about:blank respawn. Seed prefs so the
# last tab closes the window, and open a local newtab file instead of NTP.
set -euo pipefail

unset CHROMIUM_FLAGS || true
export CHROMIUM_FLAGS=""

bin=""
for candidate in \
  /usr/lib/chromium/chromium \
  /usr/lib/chromium/chrome \
  /usr/lib/chromium-browser/chromium \
  /opt/google/chrome/chrome
do
  if [[ -x "${candidate}" ]]; then
    bin="${candidate}"
    break
  fi
done
if [[ -z "${bin}" ]]; then
  echo "chromium ELF not found" >&2
  exit 1
fi

profile="${BOX_CHROME_PROFILE:-${HOME:-/home/box}/chrome-profile}"
# ENOSPC must not abort before exec: tint2 then looks like a dead click.
mkdir -p "${profile}/policies/managed" \
  "${profile}/Default" \
  "${HOME:-/home/box}/.config/chromium/policies/managed" \
  2>/dev/null || echo "warning: could not mkdir chrome profile paths" >&2
if [[ -f /etc/chromium/policies/managed/buwiz-computer.json ]]; then
  cp -f /etc/chromium/policies/managed/buwiz-computer.json \
    "${profile}/policies/managed/buwiz-computer.json" 2>/dev/null || true
  cp -f /etc/chromium/policies/managed/buwiz-computer.json \
    "${HOME:-/home/box}/.config/chromium/policies/managed/buwiz-computer.json" 2>/dev/null || true
fi

seed_chromium_profile() {
  local prefs="${profile}/Default/Preferences"
  local local_state="${profile}/Local State"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "${prefs}" "${local_state}" <<'PY'
import json, os, sys

def load(path):
    if not os.path.exists(path) or os.path.getsize(path) < 2:
        return {}
    try:
        with open(path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except Exception:
        return {}

def dump(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(data, fh, separators=(",", ":"))
    os.replace(tmp, path)

prefs_path, state_path = sys.argv[1], sys.argv[2]
prefs = load(prefs_path)
browser = prefs.setdefault("browser", {})
if isinstance(browser, dict):
    browser["close_window_with_last_tab"] = "always"
    # Openbox SSD close button (24×24), not Chromium's tiny CSD X.
    browser["custom_chrome_frame"] = False
profile = prefs.setdefault("profile", {})
if isinstance(profile, dict):
    profile["exit_type"] = "Normal"
    profile["exited_cleanly"] = True
session = prefs.setdefault("session", {})
if isinstance(session, dict):
    session["restore_on_startup"] = 5
dump(prefs_path, prefs)

state = load(state_path)
st_profile = state.setdefault("profile", {})
if isinstance(st_profile, dict):
    st_profile["exited_cleanly"] = True
    st_profile["exit_type"] = "Normal"
dump(state_path, state)
PY
    return
  fi
  if [[ ! -s "${prefs}" ]]; then
    printf '%s\n' '{"browser":{"close_window_with_last_tab":"always","custom_chrome_frame":false},"profile":{"exit_type":"Normal","exited_cleanly":true},"session":{"restore_on_startup":5}}' > "${prefs}"
  else
    sed -i 's/"exit_type":"Crashed"/"exit_type":"Normal"/g' "${prefs}" || true
  fi
  if [[ ! -s "${local_state}" ]]; then
    printf '%s\n' '{"profile":{"exited_cleanly":true,"exit_type":"Normal"}}' > "${local_state}"
  fi
}

seed_chromium_profile || true

cdp_port="${BOX_CDP_PORT:-9222}"
export DISPLAY="${DISPLAY:-${BOX_DISPLAY:-:1}}"
export GTK_CSD=0

# Mapped Chromium window (stubs are 10x10). Never xdotool --sync.
find_chrome_window() {
  command -v xdotool >/dev/null 2>&1 || return 1
  local id geo w h
  local ids
  ids="$(
    {
      xdotool search --onlyvisible --class Chromium-browser 2>/dev/null || true
      xdotool search --onlyvisible --class Chromium 2>/dev/null || true
      xdotool search --onlyvisible --name Chromium 2>/dev/null || true
    } | awk 'NF && !seen[$1]++ { print $1 }'
  )"
  for id in ${ids}; do
    geo="$(xdotool getwindowgeometry "${id}" 2>/dev/null || true)"
    if [[ "${geo}" =~ Geometry:\ ([0-9]+)x([0-9]+) ]]; then
      w="${BASH_REMATCH[1]}"
      h="${BASH_REMATCH[2]}"
      if (( w >= 200 && h >= 200 )); then
        printf '%s\n' "${id}"
        return 0
      fi
    fi
  done
  return 1
}

chrome_running() {
  pgrep -x chromium >/dev/null 2>&1
}

reap_chrome() {
  echo "reaping Chromium with no visible window" >&2
  pkill -TERM -x chromium >/dev/null 2>&1 || true
  sleep 0.4
  pkill -KILL -x chromium >/dev/null 2>&1 || true
  rm -f "${profile}/SingletonLock" "${profile}/SingletonCookie" \
    "${profile}/SingletonSocket"
}

toggle_chrome_classes=(
  --class Chromium-browser
  --class Chromium
  --class chromium
  --min-width 200
  --min-height 200
)

# CUA cook: never hide. Dock Exec stays toggle; recipes call this flag.
raise_or_launch=0
if [[ "${1:-}" == "--raise-or-launch" ]]; then
  raise_or_launch=1
  shift
fi

# Dock click (no args): hide if mapped, raise if iconic, else launch one.
# Ctrl+N / URL args do not go through this hide path.
if [[ $# -eq 0 ]] && command -v box-window-toggle >/dev/null 2>&1; then
  if [[ "${raise_or_launch}" -eq 1 ]]; then
    if box-window-toggle --raise-only "${toggle_chrome_classes[@]}"; then
      echo "raised existing Chromium window" >&2
      exit 0
    fi
  elif box-window-toggle --exists-toggle "${toggle_chrome_classes[@]}"; then
    echo "toggled existing Chromium window" >&2
    exit 0
  fi
  if chrome_running || [[ -L "${profile}/SingletonLock" ]]; then
    n=0
    while [[ ${n} -lt 10 ]]; do
      if box-window-toggle --raise-only "${toggle_chrome_classes[@]}"; then
        echo "chromium still starting; raised window" >&2
        exit 0
      fi
      sleep 0.4
      n=$((n + 1))
    done
    reap_chrome
  fi
else
  existing="$(find_chrome_window || true)"
  if [[ -n "${existing}" ]]; then
    xdotool windowactivate "${existing}" >/dev/null 2>&1 || true
    xdotool windowraise "${existing}" >/dev/null 2>&1 || true
    if [[ $# -eq 0 ]]; then
      echo "raised existing Chromium window ${existing}" >&2
      exit 0
    fi
  elif chrome_running || [[ -L "${profile}/SingletonLock" ]]; then
    reap_chrome
  fi
fi

filtered=()
has_url=0
for arg in "$@"; do
  case "${arg}" in
    --disable-setuid-sandbox|--disable-setuid-sandbox=*) ;;
    http://*|https://*|file://*|about:*|chrome://*|data:*)
      has_url=1
      filtered+=("${arg}")
      ;;
    *) filtered+=("${arg}") ;;
  esac
done

# Never about:blank. --test-type cannot load chrome://newtab/.
if [[ "${has_url}" -eq 0 && -f /usr/share/buwiz-computer/newtab.html ]]; then
  filtered+=("file:///usr/share/buwiz-computer/newtab.html")
fi

exec "${bin}" \
  --user-data-dir="${profile}" \
  --no-first-run \
  --no-default-browser-check \
  --disable-dev-shm-usage \
  --disable-gpu \
  --disable-software-rasterizer \
  --no-sandbox \
  --test-type \
  --disable-background-mode \
  --hide-crash-restore-bubble \
  --disable-session-crashed-bubble \
  --disable-features=Translate \
  --disable-infobars \
  --start-maximized \
  --window-size=1280,720 \
  --window-position=0,0 \
  --remote-debugging-address=127.0.0.1 \
  --remote-debugging-port="${cdp_port}" \
  "${filtered[@]}"
