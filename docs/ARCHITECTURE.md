# Architecture

**buwiz-computer** is the guest: a Linux image, two HTTP daemons, a CLI, and connect-only SDKs. Callers start the container themselves, then pass **exec URL + host URL + token** into the client.

[`ensurebox/`](../ensurebox/) and [`l1/`](../l1/) are **demos**. EnsureBox is not a supported production control plane.

## Product vs demos

```
Your orchestrator (or the EnsureBox demo)
    │  starts guest, remembers published URLs + BOX_TOKEN
    │  buwiz-computer.connect(execUrl, hostUrl, token)
    ▼
buwiz-computer guest (this image)
    ├── box-host :1340   identity, capabilities, ready, desktop/chrome/egress
    ├── box-exec :1337   exec + files + /v1/cua/*
    ├── box-egress-tunnel :8790  optional laptop CONNECT mux (proxy 127.0.0.1:8791)
    ├── Xvfb :1 1280×800 + openbox + x11vnc + noVNC :6080
    └── Chromium (profile volume; CDP on localhost only; optional --proxy-server)
```

The TypeScript / Python / Rust SDKs never call `docker run` and never treat `/v1/info.endpoints` as the URLs to dial. Those endpoints are **container-local listen addresses** so operators can see what the processes bound.

Demo-only path (not required to use the product):

```
 L1 demo UI  (./l1 — Win / Mac / Linux)
    │  Bearer ENSUREBOX_TOKEN only
    ▼
 EnsureBox demo  (./ensurebox)  — sample lifecycle + HTTP proxy
    │  Bearer BOX_TOKEN (never sent to L1)
    ▼
 buwiz-computer guest
```

L1 never SSHes, never holds `BOX_TOKEN`, and never talks to `box-exec` / `box-host`.

## Glossary (short)

Full list: [TERMINOLOGY.md](TERMINOLOGY.md).

| Term | Meaning |
| --- | --- |
| **buwiz-computer** | Public product name, workspace crate, and CLI binary |
| **box-exec** | In-box HTTP daemon: commands, files, CUA |
| **box-host** | In-box identity / readiness / desktop+chrome status. Does **not** run models |
| **CUA** | Computer Use against the X framebuffer **1280×800**, origin top-left |
| **SDK v1** | `connect(execUrl, hostUrl, token)` only |
| **EnsureBox / L1** | Demos of an orchestrator and a human UI |

## Ports

| Port | Process | Routes / role |
| --- | --- | --- |
| **1337** | `box-exec` | `GET /v1/health`, `POST /v1/exec`, `POST /v1/exec/stream`, files (JSON + raw), CUA, busy/metrics/shutdown |
| **1340** | `box-host` | `GET /v1/health`, `GET /v1/ready`, `GET /v1/info`, `GET /v1/desktop`, `GET /v1/desktop/windows`, `GET /v1/chrome`, `GET /v1/egress`, busy/metrics/shutdown |
| **6080** | websockify + noVNC | Viewer HTML at `/vnc.html`. Published by Compose. |
| **8790** | `box-egress-tunnel` WS | Laptop client when `BOX_EGRESS_TUNNEL=1`. Compose publishes loopback. |
| 8791 | CONNECT proxy | Chromium only, localhost. **Not published.** |
| 8792 | tunnel admin | Status probe for box-host. Localhost. **Not published.** |
| 5900 | x11vnc | RFB on **localhost only** (`BOX_VNC_BIND`). Not published. |
| 9222 | Chromium CDP | **localhost only**. Not published. |

Bind addresses: inside the guest image, `BOX_EXEC_BIND` / `BOX_HOST_BIND` default to `0.0.0.0` so Compose port-map works. Native/cargo defaults are `127.0.0.1`. Compose **host** publish is `127.0.0.1:1337` (etc.).

## Auth model

- **Shared secret:** `BOX_TOKEN`. Whoever starts the guest injects it. Required; no silent default.
- **Optional split:** `BOX_HOST_TOKEN` for `box-host` `/v1/info`, `/v1/desktop`, `/v1/chrome`, `/v1/ready`. If unset or empty, `box-host` uses `BOX_TOKEN`.
- **Header:** `Authorization: Bearer <token>`. Comparison is constant-time on equal-length tokens.
- **Unauthenticated:** `GET /v1/health` on both daemons (`{"status":"ok"}` only).
- **Authenticated:** everything else, including `/v1/ready`, file I/O, CUA, and `/v1/info`.
- **Insecure tokens:** `dev-box-token`, empty, or shorter than 16 characters are rejected unless `BOX_ALLOW_INSECURE_DEV=1` **and** both daemon binds are loopback.
- **Delivery:** as a file (`BOX_TOKEN_FILE`, `BOX_HOST_TOKEN_FILE`, `BOX_VNC_PASSWORD_FILE`, `BOX_EGRESS_TUNNEL_BEARER_FILE`) or as a value. The file form wins and is what Compose and EnsureBox use. A value passed through the container environment lands in pid 1's environ block, which `/proc/1/environ` serves to every process in the box; `unsetenv` cannot take it back out, because it edits the pointer array and not the block. See README §Auth.
- **Docker:** the entrypoint exits if neither `BOX_TOKEN` nor `BOX_TOKEN_FILE` is set. It hands each daemon a path, never a value, so no daemon has a secret in its own environ. Each daemon reads its staged file and unlinks it.
- **Residual exposure:** the file the operator mounts is readable by uid 1000, because the container healthcheck has to authenticate. Code execution in the box is compromise of `BOX_TOKEN` and `BOX_VNC_PASSWORD`.
- **Exec children:** `BOX_TOKEN`, `BOX_HOST_TOKEN`, `BOX_VNC_PASSWORD`, and their `_FILE` forms are stripped from the child environment.
- **Viewer:** noVNC is not Bearer-authenticated. x11vnc uses `BOX_VNC_PASSWORD` (first 8 characters). Independent of `BOX_TOKEN`. Host publish is loopback.
- **CORS:** default is no browser origins. Set `BOX_CORS_ORIGINS` to an explicit comma-separated list if a browser must call the guest. `*` is ignored.

WebSocket streaming for exec is **not** in this tree. Use `POST /v1/exec` (bounded output, timeout; stdout/stderr are kept on timeout) or `POST /v1/exec/stream` (NDJSON/SSE). PTY is deferred.

## Exec process lifetime

Each exec runs in its own process group (`process_group(0)`). One guard owns the group id for the life of the request, so timeout, explicit cancel, and the caller hanging up all end at the same cleanup: **SIGTERM** to the group, **SIGKILL** after `BOX_EXEC_KILL_GRACE_MS`. `DELETE /v1/exec/{id}` is the explicit form; `GET /v1/metrics` reports how many groups the daemon currently owns.

A command that finishes normally does **not** have its group killed, so `nohup myserver &` keeps running — that is the point of the pattern. What the daemon will not do is pretend it saw all the output: a background process holds the pipes open, so the response carries `output_complete: false` and `truncated: true`. Reading stops once the direct child exits rather than waiting for an EOF that no longer arrives.

## `/v1/info` URLs

`GET /v1/info` reports `endpoints.exec` / `endpoints.host` as **container-local** HTTP URLs derived from the listen binds (`0.0.0.0` → `127.0.0.1`). `endpoints.scope` is `container-local`.

That is inventory for the process inside the guest, not a second connect source of truth. SDKs and the CLI always use the URLs the caller passed to `connect`.

## Process model inside the image

```
tini
 └── entrypoint.sh
      ├── Xvfb :1  (1280x800x24)          if BOX_DESKTOP=1
      ├── openbox
      ├── x11vnc 127.0.0.1:5900
      ├── websockify/noVNC :6080
      ├── chromium (profile volume; CDP 127.0.0.1:9222)
      │     optional --proxy-server=http://127.0.0.1:8791 when BOX_EGRESS_TUNNEL=1
      │     not in the death-watch — closing the browser must not kill the box
      ├── box-egress-tunnel  (WS 8790, CONNECT 127.0.0.1:8791, admin 127.0.0.1:8792)
      │     only if BOX_EGRESS_TUNNEL=1; death-watched; start before Chromium
      ├── box-exec   (1337)
      └── box-host   (1340)  ──probes──► 127.0.0.1:1337/v1/health
                                         + X socket when desktop is required
                                         + 127.0.0.1:8792/v1/status when egress on
```

If `box-exec` or `box-host` (or the X/VNC stack) dies, the entrypoint stops siblings and the container exits so the orchestrator can restart or replace it. Chromium exiting is ignored.

Runtime user is `box` (uid 1000), not root. Ports are unprivileged.

`GET /v1/ready` is 200 when exec health succeeds, and when `BOX_DESKTOP_REQUIRED=1` the X display probe must also succeed. Chrome is **not** on the ready path. Compose healthcheck hits `/v1/ready` with `Authorization: Bearer`.

`GET /v1/info` `capabilities`:

| Flag | True when |
| --- | --- |
| `exec` | always (this binary) |
| `files` | always |
| `desktop` | X socket (and `xdpyinfo` when present) on `BOX_DISPLAY` |
| `chrome` | Chromium process and/or localhost CDP is up |
| `cua` | `BOX_CUA` enabled and the display socket exists |
| `egress_tunnel` | `BOX_EGRESS_TUNNEL`; `ready` only while a laptop client is attached |

## Path jail

`box-common::resolve_in_jail`:

1. Reject NUL and overlong paths.
2. Join relative paths to the workspace root; absolute paths must already be under the root.
3. Lexically collapse `.` / `..`.
4. `canonicalize` existing paths (follows symlinks) and require the result still be inside the root.
5. For new files, canonicalize the nearest existing ancestor and append the remainder.

`/workspace-evil` is **not** treated as inside `/workspace` (`Path::starts_with` is component-wise).

Files v1: `GET` / `PUT` / `DELETE` `/v1/files`, and `POST /v1/files/mkdir`.

## Computer Use (Linux X11)

CUA is first-party in this image:

- Screenshot: ImageMagick `import -window root` (fallback: `scrot`). JSON (base64) **or** raw `image/png` (`Accept: image/png` or `?format=png`)
- Pointer/keyboard: `xdotool` on `DISPLAY=:1`
- Coordinate space = X framebuffer **1280×800** (see `BOX_DISPLAY_GEOM`), origin top-left
- Verbs: screenshot, click, double-click, move (hover), drag, type, key, scroll, **recipe** (many of those in one HTTP call — [RECIPES.md](RECIPES.md))
- Endpoints live on `box-exec` and share `BOX_TOKEN`

**trycua** is not vendored and is not on the guest path. Windows/macOS remain clients or Docker hosts, not a native buwiz-computer OS.

## Clients

Workspace-only (do not publish):

1. **Rust** `buwiz-computer` — `BuwizComputer::connect(exec_url, host_url, token)` plus CLI
2. **TypeScript** `sdk/typescript` — same connect
3. **Python** `sdk/python` — same connect

The EnsureBox demo’s guest HTTP helper uses the TypeScript SDK. L1 still talks only to EnsureBox `/api/v1`.

## What is intentionally missing

- No model weights, no OpenAI wire on 1337/1340
- No proprietary Cursor binaries or package names
- No vendored trycua/Lume
- No `Sandbox.create()` / `docker run` in the SDK
- Optional WebSocket exec streaming
- Next.js is not on the guest path
