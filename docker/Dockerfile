# syntax=docker/dockerfile:1
# buwiz-computer: box-exec + box-host + Xvfb desktop + Chromium + CUA tools + voxl.
#
# box-exec / box-host default features compile in mimalloc (needs gcc, already
# in rust:bookworm). Opt out with cargo --no-default-features.

FROM rust:1.85-bookworm AS builder
WORKDIR /src

COPY Cargo.toml rust-toolchain.toml ./
COPY crates ./crates
COPY Cargo.lock* ./

RUN cargo build --release -p box-exec -p box-host -p box-egress-tunnel \
    && strip target/release/box-exec target/release/box-host target/release/box-egress-tunnel \
        target/release/box-keeper target/release/box-active-tab

COPY docker/shm-private.c ./
RUN gcc -shared -fPIC -O2 -o /src/target/release/libbox-shm-private.so shm-private.c

# voxl serves each screen to a browser: H.264 over a WebSocket, and /wait, /screenshot and
# /record for the agent. Pinned, because it moves fast: bump it on purpose. It stays a separate
# process (x264 is GPL) and needs a newer Rust than the box's own 1.85.
FROM rust:1.93-bookworm AS voxl-builder
ARG VOXL_VERSION=0.3.3
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config clang libclang-dev libx264-dev libyuv-dev libopus-dev libpulse-dev esbuild \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install voxl-server --version "=${VOXL_VERSION}" --locked --root /opt/voxl \
    && strip /opt/voxl/bin/voxl

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.source="https://github.com/buwiz-bot/computer" \
      org.opencontainers.image.title="buwiz-computer" \
      app.buwiz.box.desktop="1" \
      app.buwiz.box.users="1"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        tini \
        xvfb \
        # Each screen's X server takes a cookie, and only what's given it gets in.
        xauth \
        # What voxl links to. ffmpeg below already brings libx264 and libopus; these are named
        # so that dropping ffmpeg could never quietly take voxl down with it.
        libx264-164 \
        libyuv0 \
        libopus0 \
        libpulse0 \
        # A real desktop: xfce4 session with xfwm4, the panel as the dock
        # (Chromium, Files), Thunar, and the default wallpaper. No terminal: one
        # on the screen would run as the desktop's user, a click from its X
        # server and its browser profile. Commands go through box-exec. dbus-x11 is what the xfce pieces talk over. Measured at
        # +270 MB over the previous openbox-only image.
        xfce4-session \
        xfwm4 \
        xfce4-panel \
        xfdesktop4 \
        xfce4-settings \
        thunar \
        dbus-x11 \
        x11-utils \
        xfonts-base \
        fonts-liberation \
        xdotool \
        wmctrl \
        imagemagick \
        scrot \
        ffmpeg \
        chromium \
    && rm -rf /var/lib/apt/lists/* \
    # Who runs what (docs/USERS.md). box: the main screen's desktop. boxsvc: box-exec, box-host and
    # voxl. agent: a command no Bot asked for. boxx: the desktops, the only ones who may make an X
    # socket. Each Bot gets two users of its own from box-screen, from 20000 up.
    && groupadd --gid 1400 boxx && groupadd --gid 1000 box && groupadd --gid 990 boxsvc && groupadd --gid 1500 agent \
    && useradd --no-create-home --uid 1000 --gid 1000 --groups boxx --shell /bin/bash --home-dir /home/box/desktops/main box \
    && useradd --no-create-home --uid 990 --gid 990 --shell /usr/sbin/nologin --home-dir /run/box/svc boxsvc \
    && useradd --no-create-home --uid 1500 --gid 1500 --shell /bin/bash --home-dir /home/box/agents/shared agent \
    && mkdir -p /workspace /home/box /etc/chromium/policies/managed /etc/opt/chrome/policies/managed \
    # Root's on purpose, and box-prepare reads it: a volume whose top isn't root's comes from an
    # image where commands could write it, and what's in it is treated as theirs.
    && chown root:agent /workspace && chmod 1775 /workspace \
    && chown root:root /home/box && chmod 0711 /home/box

# Everything that holds a secret is execute-only. A process run from a file it cannot read is not
# dumpable, so nothing else running as its user can open its /proc/<pid>/mem or environ, or ptrace
# it, from its first instruction. voxl has to hold its password in its environment and cannot set
# this itself; the others also call PR_SET_DUMPABLE.
COPY --from=voxl-builder --chmod=0111 /opt/voxl/bin/voxl /usr/local/bin/voxl
COPY --from=builder --chmod=0111 /src/target/release/box-keeper /usr/local/bin/box-keeper
COPY --from=builder --chmod=0111 /src/target/release/box-exec /usr/local/bin/box-exec
COPY --from=builder --chmod=0111 /src/target/release/box-host /usr/local/bin/box-host
COPY --from=builder --chmod=0111 /src/target/release/box-egress-tunnel /usr/local/bin/box-egress-tunnel
# The page in front, for the server's check before it types a saved login: an extension with
# only the `tabs` permission reports it to box-active-tab over native messaging, and
# box-host serves it at GET /v1/chrome/active-tab. Every Chromium launch loads the extension.
COPY --from=builder /src/target/release/box-active-tab /usr/local/bin/box-active-tab
COPY docker/app.buwiz.active_tab.json /etc/chromium/native-messaging-hosts/app.buwiz.active_tab.json
COPY docker/active-tab /opt/box/active-tab
COPY --from=builder --chmod=0755 /src/target/release/libbox-shm-private.so /usr/lib/box/libbox-shm-private.so
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY docker/box-prepare /usr/local/bin/box-prepare
COPY docker/box-reset-desktop.sh /usr/local/bin/box-reset-desktop
COPY docker/chromium-policies.json /etc/chromium/policies/managed/buwiz-computer.json
COPY docker/chromium-policies.json /etc/opt/chrome/policies/managed/buwiz-computer.json
# The dock's Chromium launcher: same profile as the entrypoint's Chromium, so a
# click opens a window in the running instance instead of a second browser.
COPY docker/box-chromium /usr/local/bin/box-chromium
COPY docker/box-chromium-pipe /usr/local/bin/box-chromium-pipe
# A Bot's own screen on a computer several Bots share, and the once-per-change clearing of a
# profile's old extension worker that the shared screen and every own screen run.
COPY docker/box-screen /usr/local/bin/box-screen
COPY docker/box-refresh-extension /usr/local/bin/box-refresh-extension
# What a new desktop's home starts from, with the Chromium / Files launchers, so xfce4-panel never
# shows its first-run wizard and the dock is there from the first frame. box-screen and box-prepare
# copy it in: a home is on a volume, and the desktop's alone.
COPY docker/xfce/xfce4-panel.xml /opt/box/desktop-home/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml
COPY docker/xfce/xfce4-desktop.xml /opt/box/desktop-home/.config/xfce4/xfconf/xfce-perchannel-xml/xfce4-desktop.xml
COPY docker/xfce/xfwm4.xml /opt/box/desktop-home/.config/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml
COPY docker/xfce/launchers/chromium.desktop /opt/box/desktop-home/.config/xfce4/panel/launcher-1/chromium.desktop
COPY docker/xfce/launchers/files.desktop /opt/box/desktop-home/.config/xfce4/panel/launcher-2/files.desktop
# The panel and desktop menus are how a click gets to "run this". Locked for every desktop.
COPY docker/xfce/kioskrc /etc/xdg/xfce4/kiosk/kioskrc
# The desktop's wallpaper, named by xfce4-desktop.xml above.
COPY docker/xfce/wallpaper.png /usr/share/backgrounds/buwiz-computer.png
# chromium-launch.sh is intentionally not installed. Entrypoint owns Chromium.
RUN chmod 755 /usr/local/bin/entrypoint.sh /usr/local/bin/box-prepare \
        /usr/local/bin/box-reset-desktop /usr/local/bin/box-chromium \
        /usr/local/bin/box-chromium-pipe /usr/local/bin/box-active-tab \
        /usr/local/bin/box-screen /usr/local/bin/box-refresh-extension \
    # COPY keeps the mode each file has on the machine building, and git keeps only the exec bit,
    # so a checkout under umask 077 would leave these unreadable to the box user.
    && chmod -R a+rX /opt/box /etc/chromium /etc/opt/chrome /etc/xdg/xfce4/kiosk /usr/share/backgrounds

ENV WORKSPACE_ROOT=/workspace \
    BOX_EXEC_BIND=0.0.0.0:1337 \
    BOX_HOST_BIND=0.0.0.0:1340 \
    BOX_EXEC_URL=http://127.0.0.1:1337 \
    BOX_DESKTOP=1 \
    BOX_DESKTOP_REQUIRED=1 \
    BOX_DISPLAY=:1 \
    BOX_DISPLAY_GEOM=1280x800x24 \
    BOX_CHROME=1 \
    BOX_CHROME_PROFILE=/home/box/desktops/main/chrome-profile \
    BOX_CDP_PORT=9222 \
    BOX_CUA=1 \
    BOX_EGRESS_TUNNEL=0 \
    BOX_EGRESS_WS_BIND=0.0.0.0:8790 \
    BOX_EGRESS_PROXY_BIND=127.0.0.1:8791 \
    BOX_EGRESS_ADMIN_BIND=127.0.0.1:8792 \
    BOX_RUN_AS=agents \
    DISPLAY=:1 \
    RUST_LOG=info

# Root, for box-keeper alone: it sets the box up, then starts everything else as the user it should
# be. The Bot gives the container only the capabilities that takes (docs/USERS.md).
WORKDIR /workspace
EXPOSE 1337 1340 6901 6902 6903 6904 6905 8790

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/box-keeper"]
