# buwiz-computer

A sandboxed Linux computer for agents: a guest Docker image, two HTTP daemons (`box-exec` and `box-host`), a `buwiz-computer` CLI, and connect-only SDKs (Rust, TypeScript, Python).

You start the guest. Then you talk to it over HTTP with a URL pair and a bearer token. This repo does **not** publish packages to crates.io, npm, or PyPI.

License: **MIT**. MSRV: Rust **1.85**.

## What you get

| Piece | Role |
| --- | --- |
| Guest image `buwiz-computer` | Linux box: shell, files, 1280×800 X desktop, Chromium, Computer Use, a live view of every screen |
| [voxl](https://github.com/tembo/voxl) `:6901`+ | One per screen: the live view in a browser, and `/wait`, `/screenshot`, `/record` |
| `box-exec` `:1337` | Exec, files (GET/PUT/DELETE/mkdir), CUA |
| `box-host` `:1340` | Health, ready, identity, screens, desktop/chrome/egress status |
| CLI `buwiz-computer` | Same surface as the SDKs |
| SDKs | Connect with `(execUrl, hostUrl, token)` — no `docker run` helper |

Windows and macOS are clients or Docker hosts. The box OS is always this Linux image.

This tree implements **our own HTTP wire**. It is not Cursor’s `/exec-daemon` or `sand-host`.

## Quick start (guest)

```bash
cp .env.example .env            # set BOX_TOKEN and BOX_VIEWER_PASSWORD (required)
bash scripts/write-secrets.sh   # turns them into ./secrets/* for Compose to mount
docker compose up --build
```

That builds the image and starts exec, host, Xvfb, Chromium, and CUA tools. Compose publishes **1337 / 1340 / 6901-6905 on 127.0.0.1**. Reach them from another machine with an SSH tunnel, Tailscale, or similar — do not publish those ports on a public NIC.

Health (no token). Ready requires Bearer:

```bash
curl -fsS http://127.0.0.1:1337/v1/health
curl -fsS http://127.0.0.1:1340/v1/health
curl -fsS -H "Authorization: Bearer $BOX_TOKEN" http://127.0.0.1:1340/v1/ready
```

Exec (token required):

```bash
curl -fsS http://127.0.0.1:1337/v1/exec \
  -H "Authorization: Bearer $BOX_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"command":["echo","ok"]}'
```

Screenshot as JSON (base64 PNG) or raw `image/png`:

```bash
curl -fsS http://127.0.0.1:1337/v1/cua/screenshot \
  -H "Authorization: Bearer $BOX_TOKEN" \
  -X POST

curl -fsS http://127.0.0.1:1337/v1/cua/screenshot?format=png \
  -H "Authorization: Bearer $BOX_TOKEN" \
  -H "Accept: image/png" \
  -X POST \
  -o /tmp/box.png
```

CUA coordinate space is **1280×800**, origin top-left. Known click/type/key sequences should go in **one** `POST /v1/cua/recipe` (see [RECIPES.md](docs/RECIPES.md)) instead of one HTTP call per action.

A receipt's `ok` means **no step returned an error**, not that the recipe achieved anything: a taped plan whose fixed coordinates have drifted onto another window plays every step cleanly. Ask for `observe: "input"` (or `"page"`) and each step also reports the window it aimed at, where the keys were about to go, and the page URL either side of it, so a caller can tell a run that worked from a run that did not.

Live view: `http://127.0.0.1:6901/` (loopback publish). voxl asks for `BOX_VIEWER_PASSWORD`, and `?token=<password>` signs a link in. It is independent of `BOX_TOKEN`. `/healthz` is the only thing voxl answers without it. Decoding uses WebCodecs, so open it on `localhost` or over HTTPS.

```bash
./scripts/smoke.sh
```

### CLI and SDKs

After the guest is up, your orchestrator already has an exec URL, host URL, and token. Point the client at those. The SDKs **do not** start Docker and **ignore** `/v1/info` advertised URLs (those are container-local listen addresses).

```bash
cargo run -p buwiz-computer -- \
  --exec-url http://127.0.0.1:1337 \
  --host-url http://127.0.0.1:1340 \
  --token "$BOX_TOKEN" \
  exec -- echo ok
```

Workspace packages (not published):

- Rust: `crates/buwiz-computer` (library + `buwiz-computer` binary)
- TypeScript: `sdk/typescript`
- Python: `sdk/python`

### Native (no Docker, no X desktop)

```bash
./scripts/run-local.sh
# another terminal
./scripts/smoke-native.sh
cargo test --workspace
```

Native mode sets `BOX_DESKTOP=0`. CUA screenshot needs the container (or a local Xvfb).

## How an orchestrator should plug in

1. Start a container from this image. Inject `BOX_TOKEN`, `BOX_VIEWER_PASSWORD`, and `BOX_ID`. Publish **1337 / 1340 / 6901-6905** on loopback (or behind a tunnel). Do **not** publish 9222. Optional laptop egress: [EGRESS.md](docs/EGRESS.md) (`BOX_EGRESS_TUNNEL=1`, publish **8790** only, never 8791).
2. Wait until `GET <hostUrl>/v1/ready` returns 200 **with Bearer**.
3. Call `connect(execUrl, hostUrl, token)` in the CLI or an SDK. Do not parse `/v1/info.endpoints` as the public URLs.
4. Drive `POST /v1/exec`, files, `/v1/cua/*`, and `POST /v1/cua/recipe` yourself.

## Ports

| Port | Published? | Process | Notes |
| --- | --- | --- | --- |
| **1337** | host `127.0.0.1` | `box-exec` | exec, files, CUA. Process bind inside the image is `0.0.0.0`. |
| **1340** | host `127.0.0.1` | `box-host` | health, ready, info, desktop, chrome, egress. |
| **6901** | host `127.0.0.1` | `voxl` | The box's own screen. voxl's own password, not Bearer. |
| **6902–6905** | host `127.0.0.1` | `voxl` | The Bots' own screens: `6900+N` for display `:N`. |
| **8790** | host `127.0.0.1` | `box-egress-tunnel` WS | Laptop client. Off unless `BOX_EGRESS_TUNNEL=1`. See [EGRESS.md](docs/EGRESS.md). |
| 8791 | **no** | CONNECT proxy | Chromium only, `127.0.0.1` inside the image. Never publish. |
| 8792 | **no** | tunnel admin | Status for box-host. Loopback. Never publish. |
| 9222 | **no** | Chromium CDP | `127.0.0.1` only (`BOX_CDP_PORT`). Do not publish. |

Compose also sets `cap_drop: [ALL]`, `security_opt: [no-new-privileges:true]`, `pids_limit: 1024`, and `mem_limit: 4g`. That is **not** a kernel sandbox. The guest is an unprivileged uid-1000 container; Chromium still uses `--no-sandbox`.

## Environment

| Variable | Default (image) | Meaning |
| --- | --- | --- |
| `BOX_TOKEN` | **required** | Bearer token for exec, CUA, and host `/v1/info` / `/v1/ready`. No silent default. |
| `BOX_TOKEN_FILE` | unset | Path to a file holding `BOX_TOKEN`. **Preferred**: wins over `BOX_TOKEN`, and keeps the value out of `/proc/<pid>/environ`. |
| `BOX_HOST_TOKEN` | same as `BOX_TOKEN` | Optional split token for host info/desktop/chrome |
| `BOX_HOST_TOKEN_FILE` | unset | Path to a file holding `BOX_HOST_TOKEN` |
| `BOX_ALLOW_INSECURE_DEV` | unset | `1` allows short/well-known tokens **only** when both daemon binds are loopback |
| `BOX_ID` | hostname / `buwiz-computer` | Reported by `/v1/info` |
| `WORKSPACE_ROOT` | `/workspace` | Jail root for cwd and file APIs |
| `BOX_EXEC_BIND` | `0.0.0.0:1337` in image; `127.0.0.1:1337` native | Exec listen address |
| `BOX_HOST_BIND` | `0.0.0.0:1340` in image; `127.0.0.1:1340` native | Host listen address |
| `BOX_EXEC_URL` | `http://127.0.0.1:1337` | URL host uses to probe exec (container-local) |
| `BOX_CORS_ORIGINS` | empty (no browser origins) | Comma-separated allowlist; `*` is ignored |
| `BOX_DISPLAY` | `:1` | X display |
| `BOX_DISPLAY_GEOM` | `1280x800x24` | Xvfb geometry; **CUA coordinate space is 1280×800** |
| `BOX_DESKTOP` | `1` | Start Xvfb + xfce + voxl |
| `BOX_DESKTOP_REQUIRED` | `1` when desktop on | `/v1/ready` waits for the display |
| `BOX_VIEWER_PASSWORD` | **required when desktop on** | Viewer password, for every screen; independent of `BOX_TOKEN`. At least 16 characters, with no `BOX_ALLOW_INSECURE_DEV` exemption: voxl listens on every interface inside the box |
| `BOX_VIEWER_PASSWORD_FILE` | unset | Path to a file holding `BOX_VIEWER_PASSWORD`. Preferred, same reason. |
| `BOX_CHROME` | `1` | Launch Chromium on `:1` |
| `BOX_CHROME_PROFILE` | `/home/box/desktops/main/chrome-profile` | Persistent profile (compose volume) |
| `BOX_CDP_PORT` | `9222` | CDP on `127.0.0.1` only |
| `BOX_CUA` | `1` | Enable `/v1/cua/*` |
| `BOX_EGRESS_TUNNEL` | `0` | `1` starts `box-egress-tunnel` before Chromium and sets `--proxy-server`. See [EGRESS.md](docs/EGRESS.md). |
| `BOX_EGRESS_WS_BIND` | `0.0.0.0:8790` in image; `127.0.0.1:8790` native | WebSocket for the laptop client |
| `BOX_EGRESS_PROXY_BIND` | `127.0.0.1:8791` | HTTP CONNECT for Chromium. Never publish. |
| `BOX_EGRESS_ADMIN_BIND` | `127.0.0.1:8792` | Loopback status for box-host |
| `BOX_EGRESS_TUNNEL_BEARER` | required when tunnel on | WS Bearer. Prefer `_FILE`. Independent of `BOX_TOKEN`. |
| `BOX_EGRESS_TUNNEL_BEARER_FILE` | unset | Preferred file form (same story as `BOX_TOKEN_FILE`) |
| `BOX_EGRESS_RELAY_HOSTS` | empty = all | Optional CONNECT host allowlist (`*.example.com` ok) |
| `BOX_MAX_CONCURRENT_EXECS` | `8` | Max simultaneous `POST /v1/exec` (and stream/detach). Extra calls get `429 busy`. |
| `BOX_MAX_CONCURRENT_EXECS` | `8` | Max simultaneous `POST /v1/exec` (and stream/detach). Extra calls get `429 busy`. |
| `BOX_MAX_DIR_ENTRIES` | `4096` | Cap on directory listings (`truncated: true` if hit) |
| `BOX_EXEC_KILL_GRACE_MS` | `2000` | After exec timeout, wait this long after SIGTERM before SIGKILL |

## Auth

Send `Authorization: Bearer <token>`. `GET /v1/health` is public and returns only `{"status":"ok"}`. `GET /v1/ready` requires Bearer (Compose healthcheck sends it). A token must be set — as `BOX_TOKEN` or as `BOX_TOKEN_FILE`; `dev-box-token` and other short/well-known values are rejected unless `BOX_ALLOW_INSECURE_DEV=1` **and** both daemon binds are loopback.

### Delivering the secrets

Pass each secret as a **file**, not as a value: `BOX_TOKEN_FILE`, `BOX_HOST_TOKEN_FILE`, `BOX_VIEWER_PASSWORD_FILE`, `BOX_EGRESS_TUNNEL_BEARER_FILE`. The file form wins when both are set. Compose does this for you — `scripts/write-secrets.sh` writes `./secrets/*` and Compose mounts them read-only at `/run/secrets/`.

A secret handed to the container as an **environment value** is copied into pid 1's environment block at `execve`. `/proc/1/environ` serves that block to every process in the box, for the life of the box, and `docker inspect` shows it on the host. Nothing inside the box can undo that. In particular `wipe_secret_environ()` does **not**: `unsetenv` rewrites the `environ` pointer array but leaves the original block on the stack, and that block is what `/proc` reads. What it does do is keep the value out of `getenv` for the rest of the process, which is worth having but is not the same claim.

What the file form buys, precisely:

- `grep -a BOX_TOKEN= /proc/*/environ` finds nothing — not in pid 1, not in either daemon, not in Chromium.
- `docker inspect` does not show the token in `Config.Env`.
- The daemons unlink the staged copies the entrypoint writes for them, so those exist for milliseconds.
- `box-keeper`, the first process after `tini`, is the only one that reads the secrets. `entrypoint.sh` never holds one: a shell cannot be made undumpable. `box-keeper`, `box-exec`, `box-host` and `box-egress-tunnel` set `PR_SET_DUMPABLE` to 0, and they and voxl are execute-only (mode 0111): a process run from a file it cannot read is not dumpable. So nothing else running as uid 1000 can open their `/proc/<pid>/mem` or `environ`, or ptrace them. `scripts/smoke.sh` proves it from an exec child with `scripts/memory-probe.py`.
- voxl takes its password only from argv or its environment, so it gets it in its environment, which only it can read. `box-screen` asks `box-keeper` for a viewer over a root-only fifo that names a display and nothing else.

**Stage the files on a writable mount.** `box-keeper` unlinks each file it reads, so on a writable mount nothing is left once the box is up, and whoever starts the box stages them again for the next start. That is what the Bot does. Then an exec child has no file to read and no memory to read: a read-only mount is what would leave one.

What a **read-only mount** does not buy, and Compose's `secrets:` is one: the file stays for the life of the box. Its mode keeps other users out only where the mount enforces modes, and Docker Desktop's file share doesn't. Use a writable mount, or have whoever staged it remove it, as the Bot does.

Exec children never receive any of these variables, in either form.

## Layout

```
crates/box-common    path jail, bearer compare, error envelope, config, CORS
crates/box-exec      exec + files + CUA HTTP daemon
crates/box-host      identity / ready / capabilities / desktop + chrome status
crates/box-desktop   Xvfb probe, 1280×800 geometry, viewer ports, screen list
crates/box-chrome    Chromium profile + localhost CDP probe
crates/box-cua       screenshot / click / type / key / scroll / double-click / drag / move / recipe / voxl wait + record
crates/box-egress-tunnel  guest WS mux + Chromium CONNECT proxy (laptop client is the same binary)
crates/buwiz-computer      typed client + CLI (workspace only, not published)
sdk/typescript       TypeScript client (workspace only)
sdk/python           Python client (workspace only)
```

## Screens

Who runs what, and what is and isn't a wall, is in [`docs/USERS.md`](docs/USERS.md).

One computer serves one or more agents, and each agent gets a screen of its own: its own X display, input, viewer, browser profile and folder. The box's own screen is `:1`. `box-screen` adds and removes a Bot's while the computer runs, up to four (`BOX_MAX_OWN_SCREENS`), as `:2`–`:5`:

```bash
curl -fsS http://127.0.0.1:1337/v1/screen \
  -H "Authorization: Bearer $BOX_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"action":"up","bot":"amy"}'
```

`GET /v1/screens` lists them, and `?display=:N` puts any `/v1/cua/*` call on that screen. Everything an agent makes on a screen lands in that screen's folder under `/workspace`: `agents/<bot>` for a Bot's, `shared` for the box's own. Recordings, recipe cooks and Chromium downloads go there, and a terminal from a Bot's dock opens in its folder. `box-screen down` stops the screen and keeps the folder and the profile.

Measured on arm64 with a page holding about 200 MB of data open on each screen: about 510 MiB and 180 processes and threads per screen. Compose's 4 GB limit holds the box's own screen and seven Bots', so the default four leave room; at eight it runs out and the kernel starts killing browser processes. Anything that starts the image itself needs `--pids-limit 2048` too: at 1024 a fifth Bot's Chromium cannot start, and at the limit neither can an exec.

Screens keep agents apart through `/v1/cua` and the viewer; they are not a security boundary. All of them run as the box user, and anything with exec on the box can reach every screen, for one by setting `DISPLAY`. See [API](docs/API.md#screens).

## Volumes

| Mount | Role |
| --- | --- |
| `/workspace` | Jail root for cwd and file APIs, and each screen's folder. Persist across hibernate. |
| `/home/box/desktops/main/chrome-profile` | The main screen's Chromium `--user-data-dir`. Persist cookies/session. `box-prepare` gives it to the `box` user. |

## Docs

- [Deploy](docs/DEPLOY.md) — test-deploy on **Akamai Cloud** (Linode; not Vultr), then the same pattern on AWS/GCP/Azure. First test is one VM, guest Compose only, private SSH tunnel or Tailscale; CLI/SDK from your laptop. Do not expose 1337/1340/6901-6905 on the public internet.
- [Egress](docs/EGRESS.md) — laptop CONNECT tunnel for Chromium in prod (no host-network)
- [Architecture](docs/ARCHITECTURE.md)
- [Startup](docs/STARTUP.md)
- [Request processing](docs/PROCESSING.md)
- [Terminology](docs/TERMINOLOGY.md)
- [HTTP API](docs/API.md) · [OpenAPI](docs/openapi.yaml)
- [CUA recipes](docs/RECIPES.md) — one request, many pointer steps (vs reverse-web-mcp)

## What this is not

- Not a control plane: the [Bot](https://github.com/buwiz-bot/buwiz-bot) starts and stops computers
- Not L4 / not an OpenAI-compatible inference gateway
- Not a re-host of any proprietary exec/sand-host / sand-egress-tunnel binary
- Not a native Windows or macOS box OS
- Not a vendored [trycua/cua](https://github.com/trycua/cua) tree — Linux X11 is the CUA backend
- Not published to crates.io, npm, or PyPI
