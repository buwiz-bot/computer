# Architecture

**buwiz-computer** is the guest: a Linux image, two HTTP daemons, a CLI, and connect-only SDKs. Callers start the container themselves, then pass **exec URL + host URL + token** into the client.


## The guest

```
Your orchestrator (the Bot)
    │  starts guest, remembers published URLs + BOX_TOKEN
    │  buwiz-computer.connect(execUrl, hostUrl, token)
    ▼
buwiz-computer guest (this image)
    ├── box-host :1340   identity, capabilities, ready, screens, desktop/chrome/egress
    ├── box-exec :1337   exec + files + /v1/cua/* + /v1/browser/*
    ├── box-egress-tunnel :8790  optional laptop CONNECT mux (proxy 127.0.0.1:8791)
    ├── Xvfb :1 1280×800 + xfce + voxl :6901 (a Bot's own screen: :N and 6900+N)
    └── Chromium (profile in the home volume; no DevTools port; a DevTools pipe only box-exec holds; optional --proxy-server)
```

The TypeScript / Python / Rust SDKs never call `docker run` and never treat `/v1/info.endpoints` as the URLs to dial. Those endpoints are **container-local listen addresses** so operators can see what the processes bound.

## Glossary (short)

Full list: [TERMINOLOGY.md](TERMINOLOGY.md).

| Term | Meaning |
| --- | --- |
| **buwiz-computer** | Public product name, workspace crate, and CLI binary |
| **box-exec** | In-box HTTP daemon: commands, files, CUA |
| **box-host** | In-box identity / readiness / desktop+chrome status. Does **not** run models |
| **CUA** | Computer Use against the X framebuffer **1280×800**, origin top-left |
| **SDK v1** | `connect(execUrl, hostUrl, token)` only |

## Ports

| Port | Process | Routes / role |
| --- | --- | --- |
| **1337** | `box-exec` | `GET /v1/health`, `POST /v1/exec`, `POST /v1/exec/stream`, files (JSON + raw), CUA, browser, busy/metrics/shutdown |
| **1340** | `box-host` | `GET /v1/health`, `GET /v1/ready`, `GET /v1/info`, `GET /v1/desktop`, `GET /v1/desktop/windows`, `GET /v1/screens`, `GET /v1/chrome`, `GET /v1/chrome/active-tab`, `GET /v1/egress`, busy/metrics/shutdown |
| **6901–6905** | `voxl` | One per screen, `6900+N` for display `:N`: the live view, and `/wait`, `/screenshot`, `/record`. Published by Compose on loopback. |
| **8790** | `box-egress-tunnel` WS | Laptop client when `BOX_EGRESS_TUNNEL=1`. Compose publishes loopback. |
| 8791 | CONNECT proxy | Chromium only, localhost. **Not published.** |
| 8792 | tunnel admin | Status probe for box-host. Localhost. **Not published.** |

Bind addresses: inside the guest image, `BOX_EXEC_BIND` / `BOX_HOST_BIND` default to `0.0.0.0` so Compose port-map works. Native/cargo defaults are `127.0.0.1`. Compose **host** publish is `127.0.0.1:1337` (etc.).

## Auth model

- **Shared secret:** `BOX_TOKEN`. Whoever starts the guest injects it. Required; no silent default.
- **Optional split:** `BOX_HOST_TOKEN` for every `box-host` route but `/v1/health`, `/v1/ready` included: one check guards them all. When set it replaces `BOX_TOKEN` there, and `box-exec` keeps `BOX_TOKEN`. If unset or empty, `box-host` uses `BOX_TOKEN`.
- **Header:** `Authorization: Bearer <token>`. Comparison is constant-time on equal-length tokens.
- **Unauthenticated:** `GET /v1/health` on both daemons (`{"status":"ok"}` only).
- **Authenticated:** everything else, including `/v1/ready`, file I/O, CUA, and `/v1/info`.
- **Insecure tokens:** `dev-box-token`, empty, or shorter than 16 characters are rejected unless `BOX_ALLOW_INSECURE_DEV=1` **and** both daemon binds are loopback. A `BOX_TOKEN` or `BOX_VIEWER_PASSWORD` containing `replace-with` (the `.env.example` placeholders) is refused even then. The viewer password has no insecure-dev exemption at all.
- **Delivery:** as a file (`BOX_TOKEN_FILE`, `BOX_HOST_TOKEN_FILE`, `BOX_VIEWER_PASSWORD_FILE`, `BOX_EGRESS_TUNNEL_BEARER_FILE`) or as a value. The file form wins and is what Compose uses. A value passed through the container environment lands in pid 1's environ block, which `/proc/1/environ` serves to every process in the box; `unsetenv` cannot take it back out, because it edits the pointer array and not the block. See README §Auth.
- **Docker:** `box-keeper` exits if neither `BOX_TOKEN` nor `BOX_TOKEN_FILE` is set. It is the only process that reads the secrets, and it unlinks each file it reads. It hands each daemon a path, never a value, so no daemon has a secret in its own environ, and each daemon reads its staged file and unlinks it. `box-exec` starts last, once `box-host` has unlinked its copies, so no exec child can exist while a staged copy does.
- **Memory:** every process holding a secret is undumpable: execute-only binaries, and `PR_SET_DUMPABLE` 0 in our own. `entrypoint.sh` holds none. Exec children run as another user, and could not read them through `/proc/<pid>/mem` or ptrace. `scripts/memory-probe.py` checks it on every smoke.
- **Residual exposure:** a file on a read-only mount stays until whoever staged it removes it, because `box-keeper` cannot unlink it. A value passed through the environment stays in `/proc/1/environ`. With files on a writable mount, unlinked at start, neither applies. Code execution in the box still reaches every screen, every folder, and Chromium.
- **Exec children:** `BOX_TOKEN`, `BOX_HOST_TOKEN`, `BOX_VIEWER_PASSWORD`, and their `_FILE` forms are stripped from the child environment.
- **Viewer:** voxl has its own login, with `BOX_VIEWER_PASSWORD`, independent of `BOX_TOKEN`; only `/healthz` and the viewer page's static assets load without it, and a session needs the password. Host publish is loopback. `box-exec` holds the password to reach `/wait` and `/record`; `box-host` does not have it. See README §Auth.
- **CORS:** default is no browser origins. Set `BOX_CORS_ORIGINS` to an explicit comma-separated list if a browser must call the guest. `*` is ignored.

WebSocket streaming for exec is **not** in this tree. Use `POST /v1/exec` (bounded output, timeout; stdout/stderr are kept on timeout) or `POST /v1/exec/stream` (NDJSON/SSE). PTY is deferred.

## Exec process lifetime

Each exec runs in its own session, and so its own process group. One guard owns the group id for the life of the request, so timeout, explicit cancel, and the caller hanging up all end at the same cleanup: **SIGTERM** to the group, **SIGKILL** after `BOX_EXEC_KILL_GRACE_MS`. `DELETE /v1/exec/{id}` is the explicit form; `GET /v1/metrics` reports how many groups the daemon currently owns.

A command that finishes normally does **not** have its group killed, so `nohup myserver &` keeps running — that is the point of the pattern. What the daemon will not do is pretend it saw all the output: a background process holds the pipes open, so the response carries `output_complete: false` and `truncated: true`. Reading stops once the direct child exits rather than waiting for an EOF that no longer arrives.

## `/v1/info` URLs

`GET /v1/info` reports `endpoints.exec` / `endpoints.host` as **container-local** HTTP URLs derived from the listen binds (`0.0.0.0` → `127.0.0.1`). `endpoints.scope` is `container-local`.

That is inventory for the process inside the guest, not a second connect source of truth. SDKs and the CLI always use the URLs the caller passed to `connect`.

## Process model inside the image

```
tini
 └── box-keeper                            holds the secrets; not dumpable
      ├── entrypoint.sh                    the desktop; holds no secret
      │    ├── Xvfb :1  (1280x800x24)      if BOX_DESKTOP=1
      │    ├── xfce4-session
      │    ├── box-chrome-pipe → chromium (profile in the home volume; no DevTools port; --remote-debugging-pipe,
      │    │     the other end offered once at /run/box/x/<slot>/run/cdp.sock, to box-exec alone)
      │    │     optional --proxy-server=http://127.0.0.1:8791 when BOX_EGRESS_TUNNEL=1
      │    │     not in the death-watch — closing the browser must not kill the box
      │    └── box-egress-tunnel  (WS 8790, CONNECT 127.0.0.1:8791, admin 127.0.0.1:8792)
      │          only if BOX_EGRESS_TUNNEL=1; death-watched; start before Chromium
      ├── voxl :6901, and 6900+N for each Bot's screen box-screen asks for
      ├── box-host   (1340)  ──probes──► 127.0.0.1:1337/v1/health
      │                                  + X socket when desktop is required
      │                                  + 127.0.0.1:8792/v1/status when egress on
      └── box-exec   (1337)
```

If `box-exec`, `box-host` or the X stack dies, `box-keeper` stops the rest and the container exits so the orchestrator can restart or replace it. Chromium exiting is ignored, and a voxl that dies is started again after 1 s, doubling to 30 s: the box's work does not depend on anyone watching.

`box-keeper` is root, and the only thing that stays root. Everything else runs as a user of its own: [`USERS.md`](USERS.md). Ports below 6906 take a capability only the services have.

`GET /v1/ready` is 200 when exec health succeeds, and when `BOX_DESKTOP_REQUIRED=1` the X display probe must also succeed. Chrome is **not** on the ready path. The Compose healthcheck does not use it: it checks the public `/v1/health` on both daemons with no token, so the token is never in `curl`'s argv. The X probe has a time limit, and a missing `xdpyinfo` means the display is not ready.

## Helpers

Small programs in the image that the daemons and the entrypoint start. None listens on a port.

| Helper | Run by | Does |
| --- | --- | --- |
| `box-active-tab` | Chromium, as its native-messaging host (`docker/active-tab` extension) | Keeps the front tab's URL, the kind of box focused and a report count in `/run/box/x/<slot>/run/active-tab.json`, and removes the file when Chromium closes the port. `GET /v1/chrome/active-tab` reads it back. |
| `box-refresh-extension` | the entrypoint before the main screen's Chromium, `box-screen` before a Bot's | Clears the profile's service workers once when the extension's files changed, so Chromium does not keep running the old worker. On a new profile it also sets the download folder to the screen's own (`BOX_SCREEN_WORKSPACE`). |
| `box-chrome-pipe` | the entrypoint's restart loop, `box-chromium` | Starts a screen's Chromium as its desktop user with `--remote-debugging-pipe` on one end of a socket pair, and hands the other end once, over `cdp.sock` in the screen's run folder, to a `boxsvc` peer: [Browser](#browser-devtools-pipe). Lives as long as Chromium and exits as it does. |
| `box-screen` | `box-keeper`, as root, for `POST /v1/screen` | `up`, `down`, `forget`, `open` and `list` for a Bot's own screen: its users, X server, session, voxl and Chromium. See [USERS.md](USERS.md). |

`GET /v1/info` `capabilities`:

| Flag | True when |
| --- | --- |
| `exec` | always (this binary) |
| `files` | always |
| `desktop` | X socket and an `xdpyinfo` answer on `BOX_DISPLAY`. The probe has a time limit, and a missing `xdpyinfo` means not ready |
| `chrome` | The Chromium process is up |
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

- Screenshot: a native X `GetImage` on a held connection, encoded to PNG. ImageMagick `import -window root` (then `scrot`) only if that fails. JSON (base64) **or** raw `image/png` (`Accept: image/png` or `?format=png`)
- Pointer/keyboard: native XTEST on a persistent X connection. `xdotool` is the fallback when that fails
- Coordinate space = X framebuffer **1280×800** (see `BOX_DISPLAY_GEOM`), origin top-left
- Verbs: screenshot, click, double-click, move (hover), drag, type, key, scroll, **recipe** (many of those in one HTTP call — [RECIPES.md](RECIPES.md))
- Endpoints live on `box-exec` and share `BOX_TOKEN`. `?display=:N` puts a call on a Bot's own screen
- Widgets as text: `POST /v1/cua/tree` and `POST /v1/cua/element`, over AT-SPI (below)

**trycua** is not vendored and is not on the guest path. Windows/macOS remain clients or Docker hosts, not a native buwiz-computer OS.

## Accessibility tree (AT-SPI)

`POST /v1/cua/tree` and `POST /v1/cua/element` ([API.md](API.md#post-v1cuatree)) read a screen's widgets as text and act on them by `ref`. They are `box-exec` routes, in-process: the `box-a11y` crate speaks AT-SPI over D-Bus with the `atspi` crate's proxies (zbus), and there is no second daemon.

**One accessibility bus per screen.** `docker/box-a11y-bus` starts it as the screen's desktop user, before the session (from `entrypoint.sh` for the main screen, from `box-screen` for a Bot's), with the AT-SPI registry on it, and exports `AT_SPI_BUS_ADDRESS` to everything the session starts. GTK's bridge and Chromium's read that address first, and Chromium publishes its own buttons and pages because it runs with `--force-renderer-accessibility` and `ACCESSIBILITY_ENABLED=1`. The image removes `at-spi-bus-launcher`'s autostart and D-Bus activation, so there's no second bus in the desktop's private runtime folder, where `box-exec` couldn't read it.

**Who can reach it.** The bus listens on `/run/box/x/<slot>/run/a11y-bus`. That folder is the desktop user's, group `boxsvc`, mode 0750, so `box-exec` can enter it and a command's user (`agent`, a Bot's agent user) can't. The socket is 0666 inside it, because the desktop user can't give it to `boxsvc`'s group. The bus's policy (`docker/a11y-bus.conf`) lets in the desktop user, who owns it, and `boxsvc`, and no one else, and allows no eavesdropping. Two walls, then, the folder and the policy, and the rule that commands can't reach a desktop holds: a command can't read a tree or press a button any more than it can connect to X. `box-exec` is the trusted daemon here, as it is for X.

**What's checked is what's spoken to.** As with X, `box-exec` connects, then asks the kernel who listens (`SO_PEERCRED`) and refuses unless it's that screen's desktop user, before it sends a byte. A desktop's applications are not trusted either: they can answer with anything, or not at all, so every answer is bounded by the request's node, depth and time limits, every string is clipped, each call has a one-second limit, and refs are kept only for each screen's latest tree.

**Coordinates.** `bounds` are AT-SPI's screen coordinates. Each screen is its own X server at 0,0, so they are the pixels `/v1/cua/click` takes, and `element` `click` clicks the middle of an element's bounds as they are when it's asked.

**The focused window.** X knows which window has the keyboard focus better than AT-SPI does: Chromium never marks its frame `active`, and xfwm4 keeps an offscreen helper window that always is. So `focused_window` is the window named like the focused X window's title (read as recipes' `observe` reads it), then one AT-SPI calls active, and never a window off the screen.

## Browser (DevTools pipe)

**Who holds the pipe.** Every screen's Chromium is started through `box-chrome-pipe`: by the entrypoint's restart loop for the main screen, and by `box-chromium` (the dock, `box-screen open`) for any screen. It runs as the screen's desktop user, makes a socket pair, and starts Chromium with `--remote-debugging-pipe` and one end on fds 3 and 4. It keeps the other end and listens at `/run/box/x/<slot>/run/cdp.sock`, in the folder only the desktop and `boxsvc` may enter. The first peer the kernel says is `boxsvc` gets the end (`SCM_RIGHTS`). The launcher then closes its own copy and removes the socket. Anyone else is hung up on. A lock next to the socket (`cdp.lock`) makes one launcher per screen: a second start, which Chromium turns into a window of the running instance, gets no pipe and touches nothing.

**Who takes it.** `box-exec` connects when a `/v1/browser/*` call first needs the screen. It checks that the listener is that screen's desktop user (`SO_PEERCRED`) before it reads, and takes exactly one fd. The fd goes to the screen's own thread in `box-browser`, which owns it, and everything spoken over it, for as long as Chromium runs. When Chromium exits, the pipe closes: the call that finds it answers `503 browser_unavailable`, and the next one takes the new Chromium's pipe from its launcher (the image smoke kills Chromium and checks this).

**Ending it.** Checked live: Chromium quits about a second after the other end of its pipe closes, **with status 0**. The restart loop takes a 0 for a person closing the browser and doesn't start another. So `box-exec` lets go of a live pipe only when it means to: a call that panics loses its snapshot, never the pipe. `box-exec`'s shutdown sends every screen's Chromium `Browser.close` over its pipe, within 3 s. The launcher runs Chromium in a process group of its own. A signal it's sent (the entrypoint's shutdown, `box-screen down`) goes to that group, and if Chromium is still there 3 s later the group is killed. When Chromium exits for any reason, whatever is left of its group is killed too. A frame over the pipe's 64 MiB cap costs only the call it answers (`502 browser_backend`): the pipe and the screen's other tabs go on.

**What it respects.** Every flag the entrypoint and `box-chromium` already pass stays: the profile, the active-tab extension, the egress proxy flags, `--force-renderer-accessibility`. The restart loop and `chrome.pid` see the launcher, which passes on signals and exits with Chromium's status. `BOX_CHROME=0` starts no Chromium. `BOX_BROWSER=0` turns `/v1/browser/*` off on every screen, and starts the main screen's Chromium with no pipe. The launcher fails closed: if the run folder is open to others, or there is no `boxsvc` user, Chromium starts as before, with no pipe.

**What drives it.** The `box-browser` crate, in `box-exec`. Each screen gets a thread of its own the first time a call needs it. That thread owns the screen's pipe and the ultra-instinct objects on it, which are not `Send`, and runs the screen's calls one at a time; the async routes send it jobs and wait for the answer within each call's budget. It calls a fixed few of ultra-instinct's tools: the page's state, navigate, click, type, select, key, scroll into view, focus, the tab tools, wait, screenshot, save and load state, and its goal loop. Never the ones that read or set cookies, storage, headers or the network, or download. Their answers are read into `box-browser`'s own shapes, never passed on. The routes: [API.md](API.md#browser-v1browser).

**Where ultra-instinct plugs in.** `box_browser::seam` is the only module that knows ultra-instinct's types (pinned to 0ff09c4 on its main; its `docs/embedding.md` is the guide). Its `Tools` trait is a tool call by name (JSON in, JSON or a coded failure out), the policy, and a goal run. `seam::open(pipe)` puts ultra-instinct's `PipeBrowser` on the screen's socket (one socket, so the same end is its reader and its writer; ultra-instinct makes the writer non-blocking, and no other copy of the socket is kept), with a 64 MiB frame cap and a 20 s deadline per CDP call, and serves it with `Server::with_pipe`: the tab tools go over the pipe's own `Target.*`, and the first call binds the first page tab. The server is built with `require_snapshot(true)`, so every index press names the `browser_get_state` snapshot it was read in and is refused `stale_ref` when that element moved or changed; with downloads and uploads off; with an 8 MiB screenshot cap (past it the shot is taken again as a JPEG); and with an allow-list of the tools box-browser calls (`seam::ALLOWED`): never a script, the DOM, cookies, storage, headers, the network, a download, an upload, a PDF, or the keyboard-only tools: every key box-browser sends names an element. A click takes ultra-instinct's default, a real pointer press (`Input.dispatchMouseEvent` move, press and release at the element's centre, after the same gate and ticket); box-browser never sends `mouse: false`, ultra-instinct's DOM script click. It doesn't use `state: true` on its inputs or `bind_to_latest_look`: every press already names the snapshot box-browser's own look just read. The allow-list is checked by box-browser first (`policy::AllowList::check`, before any navigation, input, or goal run on the current page), and it is the stricter of the two: ultra-instinct's lets a `blob:` page through by its origin, and box-browser's refuses every `blob:`; a page a goal run reaches on its own is ultra-instinct's alone to check. The policy is set on the live server (`set_policy`) before each call that needs it; a confirm is for that call alone. Every failure is read from ultra-instinct's stable error `code`, never its text. A key press's confirm is the guard's to ask for when it would send a form: box-browser asks only for a key on a named element, and turns the guard's `needs_confirm` into a token like its own. A goal run is `Server::run_goal`: its report's steps and outcome are kept, with why it was blocked (`BlockKind`), and what it typed is dropped; one refused before its loop began is that call's failure.

What's a wall here and what isn't: [USERS.md](USERS.md).

## Clients

Workspace-only (do not publish):

1. **Rust** `buwiz-computer` — `BuwizComputer::connect(exec_url, host_url, token)?` (it returns a `Result`) plus CLI. Every request is bounded by `DEFAULT_TIMEOUT` (120 s) unless `BuwizComputer::with_timeout(Duration)` sets another; an exec with `timeout_ms` gets that plus `EXEC_MARGIN` (35 s) if longer, a `cua_wait` its wait plus `WAIT_MARGIN` (5 s), a `cua_tree` its `timeout_ms` plus `TREE_MARGIN` (5 s), a `browser_wait` or `browser_run` its `timeout_ms` plus `BROWSER_MARGIN` (20 s). A file call gets one more second for every `TRANSFER_FLOOR` (64 KiB) sent and, by its `content-length`, received; a read with no `content-length` gets the timeout between pieces instead. Running out is `Error::Timeout`. The CLI's global flag is `--request-timeout-secs` (env `BUWIZ_COMPUTER_REQUEST_TIMEOUT_SECS`). CLI: [CLI.md](CLI.md)
2. **TypeScript** `sdk/typescript` — same connect
3. **Python** `sdk/python` — same connect


## What is intentionally missing

- No model weights, no OpenAI wire on 1337/1340
- No proprietary Cursor binaries or package names
- No vendored trycua/Lume
- No `Sandbox.create()` / `docker run` in the SDK
- Optional WebSocket exec streaming
- Next.js is not on the guest path
