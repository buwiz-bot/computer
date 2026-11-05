# buwiz-computer API contract

Base URLs (Compose defaults — **your** published URLs, not `/v1/info`):

- Exec: `http://127.0.0.1:1337`
- Host: `http://127.0.0.1:1340`
- Live view: `http://127.0.0.1:6901/` (voxl; `6900+N` for display `:N`)

SDKs and the CLI are connect-only: pass those three values (exec URL, host URL, token). They ignore advertised `/v1/info` URLs.

Machine-readable spec: [openapi.yaml](openapi.yaml).

Auth: `Authorization: Bearer <BOX_TOKEN>` unless noted. `GET /v1/health` is the only public guest probe (`{"status":"ok"}`). `GET /v1/ready` requires Bearer. Errors:

```json
{
  "error": {
    "code": "unauthorized",
    "message": "missing or invalid bearer token",
    "status": 401
  }
}
```

Common codes: `unauthorized`, `invalid_request`, `path_escape`, `not_found`, `payload_too_large`, `exec_failed`, `io_error`, `internal`, `not_ready`, `cua_disabled`, `display_unavailable`, `out_of_range`, `cua_backend`.

CUA coordinate space is the X framebuffer **1280×800** (`BOX_DISPLAY_GEOM=1280x800x24`). Origin is top-left. Clicks outside that range return `400 out_of_range`.

CORS is not permissive. Set `BOX_CORS_ORIGINS` to an explicit allowlist if a browser must call the guest.

---

## box-exec `:1337`

### `GET /v1/health` (public)

```json
{ "status": "ok" }
```

No service or version fields. This is a liveness probe, not an inventory.

### `POST /v1/exec`

Run a process. `cwd` defaults to the workspace root and is jail-checked.

```json
{
  "command": ["echo", "ok"],
  "cwd": "",
  "timeout_ms": 30000,
  "env": { "FOO": "bar" },
  "stdin": null
}
```

`command` may also be a shell string: `"echo ok"` → `/bin/sh -c 'echo ok'`.

```json
{
  "stdout": "ok\n",
  "stderr": "",
  "exit_code": 0,
  "timed_out": false,
  "duration_ms": 4,
  "truncated": false,
  "output_complete": true,
  "cwd": "/workspace"
}
```

On timeout: `timed_out: true`; the process group gets **SIGTERM**, then **SIGKILL** after `BOX_EXEC_KILL_GRACE_MS` (default 2s). `exit_code` is the wait status if the child dies on SIGTERM (often 143) or `null` if SIGKILL was required. **Captured stdout/stderr are kept.** Output streams are capped (`BOX_MAX_OUTPUT_BYTES`, default 8 MiB); `truncated` is true if a cap hit. After writing `stdin`, the pipe is closed so the child sees EOF. `BOX_TOKEN`, `BOX_HOST_TOKEN`, `BOX_VIEWER_PASSWORD`, and their `_FILE` forms are stripped from the child environment.

#### Commands that leave something running

A backgrounded process inherits the write ends of stdout and stderr, so those pipes never reach EOF. Once the direct child exits, the daemon collects what is already buffered and stops reading; it does not wait for an EOF that is not coming.

- `output_complete: false` — a process the command left running still holds the pipes. Everything the foreground wrote is in `stdout` / `stderr`; anything written **after** the foreground exited was not captured.
- `truncated` is `true` whenever `output_complete` is `false`, so a caller that only looks at `truncated` is not told the output is whole when it is not.

```bash
curl -fsS "$EXEC/v1/exec" -H "Authorization: Bearer $BOX_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"command":"echo A; (sleep 300 &); echo B"}'
# {"stdout":"A\nB\n", ..., "truncated":true, "output_complete":false}
```

To run something that outlives the request, use `detach: true` and poll `GET /v1/exec/{id}`, or stream it with `POST /v1/exec/stream`. To stop it, `DELETE /v1/exec/{id}`.

Default timeout 30s; max 10 minutes (`BOX_DEFAULT_TIMEOUT_MS`, `BOX_MAX_TIMEOUT_MS`). Concurrent execs are capped (`BOX_MAX_CONCURRENT_EXECS`, default **8**); extra calls return **429** `busy`. On timeout the process group gets **SIGTERM**, then **SIGKILL** after `BOX_EXEC_KILL_GRACE_MS` (default 2s). Responses include `exec_id`.

`pty: true` is **not implemented** (400). Use `POST /v1/exec/stream` for incremental stdout/stderr.

### `DELETE /v1/exec/{id}`

Stop a running exec. Sends **SIGTERM** to its whole process group, then **SIGKILL** after `BOX_EXEC_KILL_GRACE_MS`. Works for a detached exec (whose id comes back immediately) and for an in-flight one.

```json
{ "exec_id": "exec-…", "cancelled": true }
```

`cancelled: false` means the id is known but the exec had already finished. Unknown ids return **404**. An in-flight `POST /v1/exec` that is cancelled this way still answers its own request: `timed_out` stays `false` and `exit_code` is the signal-terminated status (usually `null`), because the command did not run out of time — it was stopped.

Dropping the HTTP connection also terminates the process group — for `/v1/exec` and for `/v1/exec/stream` — so a cancelled turn does not strand a process tree. That is a side effect of hanging up, though; `DELETE` is the way to say it on purpose.

### `POST /v1/exec/stream`

Same body as `/v1/exec`. Response is `application/x-ndjson` (or SSE if `Accept: text/event-stream`): `stdout` / `stderr` chunks, then `{ "type": "exit", ... }`. The exit event carries the same `truncated` and `output_complete` flags as `/v1/exec`.

Optional `detach: true` on `/v1/exec` returns immediately with `status: "running"`; poll `GET /v1/exec/{id}`.

### `GET /v1/busy` / `GET /v1/metrics` / `POST /v1/shutdown`

Exec slots and a lightweight uptime/counters snapshot. `POST /v1/shutdown` schedules a graceful process exit (also on box-host).

### `GET /v1/files?path=&encoding=`

`path` is relative to `/workspace` or an absolute path under it. Empty `path` lists the workspace root.

File:

```json
{
  "kind": "file",
  "path": "/workspace/notes.txt",
  "size": 5,
  "encoding": "utf8",
  "content": "hello"
}
```

Directory:

```json
{
  "kind": "directory",
  "path": "/workspace",
  "entries": [{ "name": "notes.txt", "kind": "file", "size": 5 }]
}
```

`encoding=utf8` (default) or `base64`. Invalid UTF-8 files are returned as `base64`. Directory listings include `truncated` when `BOX_MAX_DIR_ENTRIES` (default 4096) is hit.

Raw bytes: `GET` / `PUT /v1/files/raw?path=` with `application/octet-stream`.

### `PUT /v1/files`

```json
{
  "path": "notes/hello.txt",
  "content": "hello",
  "encoding": "utf8",
  "create_dirs": true
}
```

```json
{ "path": "/workspace/notes/hello.txt", "bytes_written": 5 }
```

Max size: `BOX_MAX_FILE_BYTES` (default 10 MiB).

### `DELETE /v1/files?path=&recursive=`

Deletes a file or directory inside the jail. The workspace root cannot be deleted. Non-empty directories require `recursive=true`.

```json
{ "path": "/workspace/notes/hello.txt", "deleted": true }
```

### `POST /v1/files/mkdir`

```json
{ "path": "notes/sub", "parents": true }
```

```json
{ "path": "/workspace/notes/sub", "created": true }
```

`parents` defaults to true. If the directory already exists, `created` is false.

### `POST /v1/files/rename`

```json
{ "from": "old.txt", "to": "notes/new.txt" }
```

```json
{ "from": "/workspace/old.txt", "to": "/workspace/notes/new.txt" }
```

Both paths are jail-checked. Destination must not already exist. The workspace root cannot be renamed.

### `POST /v1/cua/screenshot`

Capture the root window of `BOX_DISPLAY` as PNG.

**JSON** (default): omit `Accept` or send `application/json`.

```json
{
  "encoding": "base64",
  "mime": "image/png",
  "width": 1280,
  "height": 800,
  "bytes": 41200,
  "png_base64": "iVBORw0KGgo..."
}
```

**Raw PNG:** `Accept: image/png` or `?format=png`. Body is `image/png` bytes.

`503 cua_disabled` if `BOX_CUA=0`. `503 display_unavailable` if Xvfb is down. `502 cua_backend` if GetImage / `import` / `scrot` fail.

### `POST /v1/cua/click`

```json
{ "x": 640, "y": 400, "button": 1 }
```

`button` is optional (default 1 = left; X buttons 1–7). Implemented as non-sync `mousemove` + `mousedown`, a short gap, then `mouseup`. Never `xdotool click` (100ms/press) or `mousemove --sync` (15s stall). `200 {"ok": true}`.

### `POST /v1/cua/press`

Alias: `POST /v1/cua/mousedown`.

```json
{ "x": 200, "y": 40, "button": 1 }
```

`mousemove` then `mousedown`. No `mouseup`. Pair with `move` (button still down) and `mouseup`.

### `POST /v1/cua/release`

Alias: `POST /v1/cua/mouseup`.

```json
{ "x": 500, "y": 200, "button": 1, "path": [{ "x": 220, "y": 40 }, { "x": 400, "y": 120 }] }
```

Optional motion `path` (max 64 points) then `mouseup`. `x`/`y` must both be set or both omitted.

### `POST /v1/cua/double-click`

```json
{ "x": 640, "y": 400, "button": 1 }
```

Two clicks at the point. Same button range as click.

### `POST /v1/cua/move`

Hover; no button.

```json
{ "x": 640, "y": 400 }
```

### `POST /v1/cua/drag`

```json
{ "x1": 100, "y1": 100, "x2": 400, "y2": 300, "button": 1 }
```

Mouse down at (x1,y1), interpolated `mousemove` events, mouse up at (x2,y2). A short pause after press lets the window manager start a title-bar grab. Both points must be in range.

### `POST /v1/cua/type`

```json
{ "text": "hello" }
```

Typed via XTEST (xdotool fallback). Rejects empty or oversized payloads.

### `POST /v1/cua/key`

```json
{ "key": "Return", "action": "tap" }
```

`key` is an X11 / xdotool keysym (`Return`, `Tab`, `ctrl+c`, `shift`, …). Whitespace and `;` are rejected. `action` is `tap` (default, down+up with modifiers cleared), `down`, or `up`. Do not clear modifiers on `down`/`up` so held Ctrl/Alt/Shift/Super stay held.

### `POST /v1/cua/scroll`

```json
{ "x": 640, "y": 400, "dx": 0, "dy": 120 }
```

Moves to `(x,y)` then emits wheel **press/release** (X buttons 4–7). Never `xdotool click`. 120 units is one notch (X11); smaller non-zero deltas still emit one notch. Positive `dy` scrolls down; negative up. `dx` is horizontal. At least one of `dx`/`dy` must be non-zero.

### `POST /v1/cua/recipe`

Many CUA steps in **one** request. The guest lints the plan (empty, too many steps, out of range) and returns **400** without moving the pointer if the plan is bad. Steps then run in order (one X pointer — not a parallel DAG). Default `screenshot` is `end`. See [`RECIPES.md`](RECIPES.md) for the reverse-web-mcp comparison.

```json
{
  "name": "search",
  "stop_on_error": true,
  "screenshot": "end",
  "record": true,
  "artifact_dir": "shared/cooks/demo",
  "steps": [
    { "op": "reset_desktop" },
    { "op": "click", "x": 640, "y": 80, "button": 1 },
    { "op": "type", "text": "hello" },
    { "op": "key", "key": "Return" },
    { "op": "wait", "ms": 200 }
  ]
}
```

`200` is a receipt (`ok`, `ran`, `stopped_at`, `duration_ms`, `steps[]`, optional `screenshot`, `artifacts[]`). When `artifact_dir` is set, PNG/video are workspace files (`path` on the receipt) so a client can fetch them without inline base64. `record: true` starts x11grab **before the first CUA step** and SIGINT-stops after the last (no `-t`), then remuxes the tape to progressive `+faststart` MP4. `reset_desktop` closes guest windows on this X session. A step failure with `stop_on_error: true` is still **200** with `ok: false`. Max 256 steps. Wait max 10s per step. Optional `settle` is `off` (default: no extra Chromium/page waits, no launch/close side effects), `compressed`, or `raw`. Keys and typed characters are paced so Chromium can map them. Combined chords are one step (`{ "op": "key", "key": "ctrl+l" }`), not ctrl/l down/up.

#### What `ok` means, and what `observe` adds

`ok` means **no step returned an error**. It does not mean the recipe achieved anything. A recipe of fixed coordinates played against a desktop that has moved on delivers every step without error, so a run that did nothing and a run that worked are the same receipt. The box cannot know what a recipe was *for*, so `ok` is left alone rather than made to guess, and the receipt reports what was seen instead.

Optional `observe` is `off` (default), `input`, or `page`:

- **`off`** — the receipt this endpoint produced before the field existed, byte for byte. No probes, no cost.
- **`input`** — each step gains an `observed` block carrying `target` (the window covering the step's target coordinate, read **before** the step moved anything) for pointer steps, and `focus` (where the keys were about to go) for `type` and `key`. Both are X11 requests on a connection the box already holds — no processes are forked.
- **`page`** — everything `input` gives, plus `url_before` / `url_after` around `click`, `double_click`, `type` and `key`, read from the Chromium DevTools HTTP endpoint on loopback.

```json
{
  "index": 5,
  "op": "click",
  "ok": true,
  "ms": 14,
  "observed": {
    "target": { "id": "0x02a00003", "class": "chromium.Chromium", "title": "kabisado - YouTube" },
    "url_before": "https://www.youtube.com/results?search_query=kabisado",
    "url_after": "https://www.youtube.com/results?search_query=kabisado",
    "observe_ms": 3
  }
}
```

Reading a receipt:

- The receipt echoes `observe` at the top level, absent when it was `off`. A receipt is often read a long way from the request that produced it, and without the echo a receipt with no `observed` blocks could not be told apart from one that never asked for any.
- **No `observed` key** on a step means the box did not look — `observe` was `off`, or the step had nothing to look at (`wait`, `screenshot`, `reset_desktop`, a `release` with no coordinate).
- **An `observed` block with a field missing** means the box looked and got no answer. Absent is never "nothing was there": a window that closed mid-observation, an X server that did not answer inside 400 ms, and a Chromium that is not running all read as absent.
- `focus.state` is `none`, `pointer_root`, `root`, or `window`. **`none` means no window held the keyboard focus**, so the X server discarded the keystrokes and the `type` reached nothing at all. `root` is the window manager parking focus where nothing on this desktop listens, which is also nowhere.
- `url_after` is read as soon as the step returns (after any `settle` wait). A browser navigation is not instant, so a `Return` that *did* navigate often still shows the old URL there and the new one in the next step's `url_before`. A URL that never moves across a whole run is a `Return` that submitted nothing.
- `observe_ms` is the wall time that step spent looking rather than acting. It is **not** counted in the step's `ms`, the same way a per-step screenshot never was.

Cost, so a caller can decide rather than be surprised. `input` forks nothing: it is about **ten X round-trips** for a pointer step and about **four** for a `type`, over the box's own unix socket, on a connection held separately from the input one. That is an order of magnitude below the pacing a step already pays — `CLICK_GAP` is 12ms per click and typed characters are paced at 30ms each. `page` adds **two** DevTools HTTP calls per observed step (one each side); where Chromium is not listening, the loopback connection is refused immediately, and where it is hung, each call is capped at 250ms. Every observation is bounded at 400ms on the X side, and one that fails costs the receipt a fact, never the recipe a step. `observe` changes no timing except its own: it never waits for a page to load and it never launches anything.

---

## box-host `:1340`

### `GET /v1/health` (public)

```json
{ "status": "ok" }
```

### `GET /v1/ready` (bearer)

`200` when `box-exec` answers `/v1/health`, and (when `BOX_DESKTOP_REQUIRED=1`) the X display is up. Otherwise `503` with `error.code = not_ready`. Chrome is not required for ready. Missing or invalid Bearer is `401`.

```json
{ "status": "ready", "service": "box-host", "exec_ready": true, "desktop_ready": true }
```

### `GET /v1/info` (bearer)

Uses `BOX_HOST_TOKEN` if set, else `BOX_TOKEN`.

```json
{
  "box_id": "local-dev",
  "service": "box-host",
  "protocol": "v1",
  "version": {
    "box_host": "0.1.0",
    "box_exec": "0.1.0",
    "protocol": "v1"
  },
  "capabilities": {
    "exec": { "enabled": true, "ready": true },
    "files": { "enabled": true, "ready": true },
    "desktop": { "enabled": true, "ready": true },
    "chrome": { "enabled": true, "ready": true },
    "cua": { "enabled": true, "ready": true },
    "egress_tunnel": { "enabled": false, "ready": false }
  },
  "endpoints": {
    "exec": "http://127.0.0.1:1337",
    "host": "http://127.0.0.1:1340",
    "scope": "container-local"
  },
  "workspace": "/workspace"
}
```

`endpoints` are the listen addresses **inside the guest** (`0.0.0.0` rewritten to loopback). They are not the URLs a remote SDK should dial. Callers always pass the URLs they published.

### `GET /v1/desktop` (bearer)

```json
{
  "enabled": true,
  "ready": true,
  "available": true,
  "display": ":1",
  "geometry": "1280x800x24",
  "viewer": {
    "kind": "voxl",
    "port": 6901,
    "path": "/",
    "url": "http://127.0.0.1:6901/"
  }
}
```

Connect to `viewer.url` through a tunnel or loopback publish. voxl asks for `BOX_VIEWER_PASSWORD`, which is independent of `BOX_TOKEN`; `?token=<password>` signs a link in, and programs send `Authorization: Bearer <password>`. The URL here never carries it, and neither does any other response of this API.

### Screens

One computer serves one or more agents, each on a screen of its own. `GET /v1/info` gives `capabilities.viewer: {kind: "voxl", port, enabled, ready}` for the box's own screen.

`GET /v1/screens` (bearer):

```json
{
  "screens": [
    {"display": ":1", "slot": 1, "bot": null, "viewer_port": 6901, "state": "running", "workspace": "shared"},
    {"display": ":2", "slot": 2, "bot": "amy", "viewer_port": 6902, "state": "running", "workspace": "agents/amy"}
  ]
}
```

`state` is `running`, `viewer_down` (the display is up, so input and screenshots work, but voxl is not, or not yet) or `stopped`. `viewer_port` is the port inside the box: a caller that publishes it elsewhere maps it. The list is empty with `BOX_DESKTOP=0`.

Add and remove a Bot's screen while the computer runs with `POST /v1/exec`: `box-screen up <bot>` prints its slot once its voxl takes connections, and `box-screen down <bot>` stops it and keeps its folder and profile. Slots hold for the life of the home directory, so a Bot comes back on the same display.

`?display=:N` puts any `/v1/cua/*` call on that screen. Through `/v1/cua` and the viewer, input on one screen does not reach another: each is its own X server with its own pointer, keyboard focus and clipboard.

That isolation stops there. `POST /v1/exec` can reach any screen: its `env` can set `DISPLAY`, the X servers accept any local client, and everything runs as the box user, so an exec child can type into another agent's screen and read another agent's folder. Give an agent `/v1/cua` and not `/v1/exec` if it must stay on its screen, and a computer of its own if it must not touch another agent's work.

### `POST /v1/cua/wait` (bearer)

Returns when the screen settles or changes, from voxl's own damage tracking, so an agent need not sleep and hope. It works with no viewer open. All fields are optional and times are in ms:

```json
{"idle": 300, "change": false, "since": 812, "region": "0,0,800,600", "timeout": 10000}
```

Answer: `{"changed": true, "timed_out": false, "seq": 813, "waited_ms": 340}`. To wait for what an action caused: take `seq` first, act, then wait with `{"change": true, "since": <seq>}`, then with `{"idle": 300}`. A blinking caret redraws about twice a second, so keep `idle` under 500 or leave it out of `region`. `503 viewer_unavailable` means that screen's voxl is down.

### `POST /v1/cua/record/start` and `/stop` (bearer)

`start` answers `{"id": 3}`. `stop` takes `{"id": 3}` and answers `{"path": "agents/amy/recordings/1794000000000-3.mp4", "bytes": 182044}`, relative to the workspace, in the folder of the screen recorded. A recording is the H.264 voxl already encodes, so it costs no extra CPU. It holds frames only where the screen changed, has no cursor, and ends on its own after 300 s; `stop` must arrive within 60 s of that. Call `stop` on the same `?display=` as `start`: ids belong to that screen's voxl, and they start over when it does.

### `GET /v1/chrome` (bearer)

```json
{
  "enabled": true,
  "ready": true,
  "running": true,
  "profile": "/home/box/chrome-profile",
  "cdp": "127.0.0.1:9222",
  "display": ":1"
}
```

CDP is loopback-only. `ready` means Chromium answered `GET /json/version` on that port (not a `/proc` scrape). Agents inside the box may attach; do not publish 9222.

### `GET /v1/egress` (bearer)

Laptop CONNECT tunnel. `ready` / `client_attached` mean a `box-egress-tunnel client` is attached right now. When enabled but not ready, Chromium CONNECT fail-closes (`503`). `ws` / `proxy` are container-local listen addresses — not the URL to dial. See [EGRESS.md](EGRESS.md).

```json
{
  "enabled": true,
  "ready": false,
  "client_attached": false,
  "protocol": "box-egress-v1",
  "ws": "127.0.0.1:8790",
  "proxy": "127.0.0.1:8791"
}
```

### `GET /v1/desktop/windows` (bearer)

`wmctrl -lx` on the guest display (`id`, `desktop`, `class`, `title`). Empty if desktop is down.

Responses echo `x-request-id` (honored if sent, otherwise minted).
