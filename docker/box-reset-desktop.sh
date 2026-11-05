#!/bin/bash
# Restore a clean guest desktop without restarting Docker / X / VNC / Chromium.
# Close client windows and leftover jobs. Leave the xfce session (xfwm4, panel,
# xfdesktop, settings daemon) + x11vnc + the entrypoint Chromium process running
# (one Chromium model).
set -uo pipefail

export DISPLAY="${DISPLAY:-${BOX_DISPLAY:-:1}}"
export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-XFCE}"

uid="$(id -u)"
self="$$"

protected_comm() {
  case "$1" in
    tini|entrypoint.sh|box-exec|box-host|Xvfb|Xorg|openbox|tint2|xfce4-session|xfwm4|xfce4-panel|xfdesktop|xfsettingsd|xfconfd|x11vnc|websockify|websockify-nodelay|dbus-daemon|dbus-launch|pipewire|pipewire-pulse|pulseaudio|ssh-agent|gpg-agent|ffmpeg|sleep|chromium|chrome)
      return 0
      ;;
  esac
  return 1
}

protected_class() {
  local inst cls
  inst="${1%%.*}"
  cls="${1#*.}"
  case "${inst}" in
    tint2|Tint2|Openbox|openbox|xfdesktop|Xfdesktop|xfce4-panel|Xfce4-panel|xfwm4|Xfwm4|N/A|Chromium|chromium|Google-chrome|google-chrome|"") return 0 ;;
  esac
  case "${cls}" in
    tint2|Tint2|Openbox|openbox|xfdesktop|Xfdesktop|xfce4-panel|Xfce4-panel|xfwm4|Xfwm4|N/A|Chromium|chromium|Google-chrome|google-chrome|"") return 0 ;;
  esac
  return 1
}

close_windows() {
  local id wclass
  if ! command -v wmctrl >/dev/null 2>&1; then
    return 0
  fi
  while read -r id _ wclass _; do
    [[ "${id}" == 0x* ]] || continue
    if protected_class "${wclass}"; then
      continue
    fi
    wmctrl -ic "${id}" >/dev/null 2>&1 || true
  done < <(wmctrl -lx 2>/dev/null || true)
}

kill_gui() {
  # Close helper GUI apps. Never kill Chromium: the entrypoint owns that
  # process and will respawn it if we SIGTERM it.
  pkill -u "${uid}" -x xfce4-terminal >/dev/null 2>&1 || true
  pkill -u "${uid}" -x thunar >/dev/null 2>&1 || true
  pkill -u "${uid}" -x Thunar >/dev/null 2>&1 || true
}

kill_stray_jobs() {
  local pid ppid comm args parent_cmd
  # Entrypoint children include x11vnc, python3 websockify, box-exec, and the
  # watchdog `sleep 1` (`set -e`). Killing those exits the container.
  while read -r pid ppid comm args; do
    [[ "${pid}" =~ ^[0-9]+$ ]] || continue
    if [[ "${pid}" == "${self}" || "${ppid}" == "${self}" ]]; then
      continue
    fi
    parent_cmd=""
    if [[ -r "/proc/${ppid}/cmdline" ]]; then
      parent_cmd="$(tr '\0' ' ' < "/proc/${ppid}/cmdline" 2>/dev/null || true)"
    fi
    case "${parent_cmd}" in
      *entrypoint.sh*) continue ;;
    esac
    if protected_comm "${comm}"; then
      continue
    fi
    case "${args}" in
      *entrypoint.sh*|*websockify*|*x11vnc*|*openbox*|*tint2*)
        continue
        ;;
    esac
    case "${comm}" in
      chromium|chrome|xfce4-terminal|thunar|Thunar|xterm|uxterm|gnome-terminal|mousepad|gedit)
        # Chromium is protected_comm; this branch is for other GUI leftovers.
        if [[ "${comm}" == "chromium" || "${comm}" == "chrome" ]]; then
          continue
        fi
        kill -TERM "${pid}" >/dev/null 2>&1 || true
        ;;
      yes|curl|wget|mpv|vlc)
        kill -TERM "${pid}" >/dev/null 2>&1 || true
        ;;
    esac
  done < <(ps -u "${uid}" -o pid=,ppid=,comm=,args= 2>/dev/null || true)
}

echo "box-reset-desktop: closing windows on ${DISPLAY}" >> /tmp/box-reset-desktop.log
close_windows
sleep 0.35
kill_gui
sleep 0.2
kill_stray_jobs
sleep 0.15
kill_gui

# Dock launchers refuse a second spawn while a lock dir exists.
rm -rf /tmp/box-toggle-*.lock >/dev/null 2>&1 || true

if [[ -f /usr/share/pixmaps/buwiz-computer-wallpaper.png ]] && command -v feh >/dev/null 2>&1; then
  feh --no-fehbg --bg-fill /usr/share/pixmaps/buwiz-computer-wallpaper.png >/tmp/wallpaper.log 2>&1 || true
elif command -v xsetroot >/dev/null 2>&1; then
  xsetroot -solid "#3d6d99" >/dev/null 2>&1 || true
fi

echo "box-reset-desktop: done" >> /tmp/box-reset-desktop.log
exit 0
