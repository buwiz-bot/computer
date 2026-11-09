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

Common codes: `unauthorized`, `invalid_request`, `path_escape`, `not_found`, `payload_too_large`, `exec_failed`, `io_error`, `internal`, `not_ready`, `busy` (429, every exec slot is taken), `cua_disabled`, `display_unavailable` (503, the X server is down), `viewer_unavailable` (503, that screen's voxl is down), `out_of_range`, `cua_backend` (502), `keeper_failed` (502, `POST /v1/screen` could not reach `box-keeper`), and the browser's: `browser_disabled`, `browser_unavailable` (503), `stale_ref`, `confirm_required`, `target_gone`, `dialog_open` (409), `domain_not_allowed` (403), `action_refused` (422), `browser_backend` (502), `browser_timeout` (504).

A few errors carry `error.details`, what the caller needs to act on them: `confirm_required` has `{label, confirm}`, `domain_not_allowed` has `{host}`. Every other error has none.

CUA coordinate space is the X framebuffer **1280×800** (`BOX_DISPLAY_GEOM=1280x800x24`). Origin is top-left. Clicks outside that range return `400 out_of_range`.

CORS is not permissive. Set `BOX_CORS_ORIGINS` to an explicit allowlist if a browser must call the guest.

---

## box-exec `:1337`

### `GET /v1/health` (public)

```json
{ "status": "ok" }
```

No service or version fields. This is a liveness probe, not an inventory.

All `/v1/files*` routes take `bot` the same way, in the query or the body, and work as that user.

### `POST /v1/exec`

Run a process, as a user of its own ([`USERS.md`](USERS.md)): `bot` names the Bot it's for, and a Bot this computer hasn't got is a 404. Without it, the shared `agent` user. `cwd` defaults to the workspace root and is jail-checked as that user. It gets no `DISPLAY`, and no X server would take it: only `/v1/cua/*` and `/v1/screen` reach a screen.

```json
{
  "command": ["echo", "ok"],
  "cwd": "",
  "bot": "amy",
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
- `truncated` is `true` when `output_complete` is `false` for a finished command, so a caller that only looks at `truncated` is not told the output is whole when it is not. It is also `true` when a cap hit. There are two exceptions where `output_complete` is `false` and `truncated` is `false`, because nothing was read at all: polling a detached exec that is still running, and a stream whose command failed to start (its `exit` event follows an `exec failed:` line on stderr).

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
- **`page`** — everything `input` gives, plus `url_before` / `url_after` around `click`, `double_click`, `type` and `key`, read from that screen's extension report (the same one `GET /v1/chrome/active-tab` serves). There is no DevTools port.

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

Cost, so a caller can decide rather than be surprised. `input` forks nothing: it is about **ten X round-trips** for a pointer step and about **four** for a `type`, over the box's own unix socket, on a connection held separately from the input one. That is an order of magnitude below the pacing a step already pays — `CLICK_GAP` is 12ms per click and typed characters are paced at 30ms each. `page` adds **two** reads of the screen's small report file per observed step (one each side); where the browser has not reported, the read finds nothing and the URL is absent. Every observation is bounded at 400ms on the X side, and one that fails costs the receipt a fact, never the recipe a step. `observe` changes no timing except its own: it never waits for a page to load and it never launches anything.

### `POST /v1/cua/tree`

A screen's accessibility tree, over AT-SPI: native apps' buttons, fields, menus and tabs as text, Chromium's own toolbar and pages included, so an agent can read what's on the screen and act on it by name instead of by pixels. `?display=:N` for another screen. Every field is optional, and no body is the focused window with the defaults:

```json
{ "scope": "focused_window", "max_depth": 12, "max_nodes": 400, "timeout_ms": 3000, "include_hidden": false, "compact": true }
```

- `scope`: `focused_window` (default) is the window with the keyboard focus, as X names it (its title), else the one AT-SPI calls active. `desktop` is every application with its windows.
- `max_depth` 0–40 (levels below the top nodes), `max_nodes` 1–2000, `timeout_ms` 1–10000. Out of range is `400 invalid_request`.
- `include_hidden`: nodes that are neither showing nor visible (a closed menu, a tab behind another) are left out, with what's under them, unless this is true.
- `compact`: nameless containers with nothing to act on (`panel`, `filler`, `section`, …) give their place to their children, unless this is false. Chromium wraps each control in a few of them. `max_nodes` and `max_depth` count the nodes read, compacted ones included.

```json
{
  "snapshot": 48213,
  "display": ":1",
  "scope": "desktop",
  "nodes": [
    { "ref": "48213:4", "role": "application", "name": "xfce4-panel", "states": [], "children": [
      { "ref": "48213:11", "role": "frame", "name": "", "states": ["enabled"],
        "bounds": { "x": 576, "y": 736, "width": 128, "height": 64 }, "children": [
          { "ref": "48213:35", "role": "button", "name": "Files", "states": ["enabled"],
            "bounds": { "x": 640, "y": 736, "width": 64, "height": 64 }, "actions": ["Click"] } ] } ] }
  ],
  "count": 63,
  "truncated": false,
  "truncated_by": [],
  "skipped": 0,
  "elapsed_ms": 44
}
```

- `ref` is `<snapshot>:<n>`, good for this tree only: `POST /v1/cua/element` takes refs from the screen's **latest** tree and refuses older ones.
- `role` is AT-SPI's in snake case (`push_button`, `entry`, `page_tab`, `document_web`). `name` is always there and may be empty; `description` and `text` only when they say something new. `text` is a text node's contents, clipped; a password field's never comes, nor that of a node whose role is `unknown`.
- `states` is the useful subset: `active`, `checked`, `editable`, `enabled`, `expanded`, `focusable`, `focused`, `pressed`, `selected` and a few more ([`openapi.yaml`](openapi.yaml)).
- `bounds` are screen pixels, the space `/v1/cua/click` takes; absent when the element takes no space. `actions` are AT-SPI's action names. Chromium names none of its, so its buttons list none, and `press` still runs their first, the default action.
- Bounded everywhere: names and descriptions at 160 characters, text at 400, 8 actions. The walk is breadth first, so a cut tree still has the top of every window. A cut sets `truncated`, lists the limits in `truncated_by` (`max_nodes`, `max_depth`, `time`), and puts `children_omitted` on each node that lost children. An application that hangs costs the time budget and no more. `skipped` counts nodes whose application answered with an error.
- An empty tree is not an error: it comes with a `reason` ("no application on this screen publishes an accessibility tree", or no window is focused).

Errors: `503 cua_disabled`, `503 display_unavailable`, `503 a11y_unavailable` (the screen has no accessibility bus, or what listens there isn't its desktop), `502 a11y_backend` (its registry did not answer).

### `POST /v1/cua/element`

An action on a node of the screen's latest tree, by `ref`. `?display=:N` must be the screen the tree came from.

```json
{ "ref": "48213:35", "action": "press" }
{ "ref": "48213:57", "action": "set_text", "text": "hello" }
```

- `press`: the element's own AT-SPI action, the first of `click`, `press`, `activate`, `jump`, `toggle`, `open`, else its only one, or its first when that has no name (Chromium's default action, reported as `performed: "default"`). The application says whether it took it, not whether anything happened, and some widgets only act on a real click (xfce's panel launchers are one): look again, or use `click`.
- `click`: a real click (XTEST, as `/v1/cua/click`) in the middle of the element where it is **now**, so a window that moved since the tree is still hit. The answer has `at: {x, y}`.
- `focus`: the keyboard focus, through AT-SPI.
- `set_text`: replaces an editable field's text, up to 64 KiB. Only for fields with AT-SPI's EditableText (GTK entries; not Chromium's address bar): otherwise `focus` or `click`, then `/v1/cua/type`.

`200 {"ok": true, "ref": "48213:35", "action": "press", "performed": "Click"}`. Errors: `409 stale_ref` (the ref is from an older tree, from another screen's, or its element is gone: read the tree again), `422 unsupported_action` (the element has no way to do it), `422 action_refused` (its application said no), `400 invalid_request` (a malformed ref, `set_text` without `text`, `text` with another action), `400 out_of_range` (a `click` off the screen), `502 a11y_backend`, and the `503`s of `/v1/cua/tree`.

How box-exec reaches each screen's tree, and why a command can't: [`ARCHITECTURE.md`](ARCHITECTURE.md#accessibility-tree-at-spi).

---

## Browser (`/v1/browser/*`)

Each screen's Chromium, driven over its DevTools pipe through [ultra-instinct](https://github.com/hexuria/ultra-instinct): ultra-instinct reads the page (DOM and accessibility tree), and every press goes through its gate and a one-shot ticket bound to what it saw. All eight routes are `POST` on `box-exec`, take the token, and take `?display=:N` for another screen, as `/v1/cua/*` do. The pipe is `box-exec`'s alone, never a command's: [USERS.md](USERS.md).

**Pending.** The pipe transport is being added to ultra-instinct. Until `box-exec` is built with it, every route answers `503 browser_unavailable`, and no screen's pipe is taken from its launcher.

**Limits.** Every call has a time budget (45 s; a wait or a run gets its own `timeout_ms` plus 15 s). Past it the answer is `504 browser_timeout`, and calls queue behind the one still running: past 4 waiting, `429 busy`. Every answer is bounded: labels at 160 characters, addresses at 2048, a field's text at 400, 8 actions per element, 100 tabs, a screenshot at 8 MiB, anything else at 4 MiB of JSON.

**Errors**, the same on every route:

| Status | `code` | When |
| --- | --- | --- |
| 400 | `invalid_request` | A field out of range or unknown, a malformed ref, a display that isn't `:N` |
| 403 | `domain_not_allowed` | The page, or the address asked for, is off `allow_domains`; `error.details.host` |
| 409 | `stale_ref` | The ref is from an older snapshot, or its element is gone or moved |
| 409 | `confirm_required` | The click needs a person's confirm; `error.details.{label, confirm}` |
| 409 | `target_gone` | The tab is gone: list the tabs, switch or open one, snap again |
| 409 | `dialog_open` | A JavaScript dialog holds the tab |
| 422 | `action_refused` | ultra-instinct's guard said no: covered, disabled, drifted, a spent ticket |
| 429 | `busy` | More calls wait for this screen's browser than it takes |
| 500 | `internal` | `box-exec` asked ultra-instinct for a tool it may not use: a bug in `box-exec` |
| 502 | `browser_backend` | The browser answered badly, too much, or not at all |
| 503 | `browser_disabled` | `BOX_BROWSER`, `BOX_CHROME` or `BOX_DESKTOP` is off |
| 503 | `browser_unavailable` | The screen has no browser `box-exec` can use: no transport yet, or its pipe closed (the next call takes the new Chromium's) |
| 504 | `browser_timeout` | The call's budget ran out |

`target_gone` is 409, not 404: the call named nothing wrong, the browser's state moved under it, as with `stale_ref`.

**Rules, checked by box-exec before ultra-instinct's guard checks them again:**

- **Confirm.** A click on an element whose label matches `BOX_BROWSER_CONFIRM_PRESS` (comma list of globs; default `Pay*,Delete*,Buy*,Place order*`; empty confirms nothing) is refused with `409 confirm_required`, `error.details.label` and a one-time token, `error.details.confirm`. A person approves the label, and the caller sends the token back as `confirm`. A token is good once, for that element, on that screen, for 10 minutes, and only while the label is the one approved.
- **Allowed domains.** `allow_domains` (`shop.example`, `*.shop.example`), sent with each call that gives input or goes somewhere. Off the list, `navigate` and `tabs open` refuse the address and `act` refuses the page, `403 domain_not_allowed` with `error.details.host`. Absent or empty is no restriction. It's per call, not kept per screen: the broker names the Bot and its list from its own records with every call, as it does `bot`, so nothing a screen kept from one Bot can reach the next one to use it, and a goal run's list covers every step of it.
- **Passwords.** A password field's text is never returned. `value` comes only for a field whose input type is known and isn't `password` or `hidden`; a field that is, or may be, a password's is `sensitive: true`.

### `POST /v1/browser/snap`

```json
{ "max_nodes": 200 }
```

```json
{
  "snapshot": 48213,
  "display": ":1",
  "url": "https://shop.example/login",
  "title": "Sign in",
  "elements": [
    { "ref": "@e1", "role": "textbox", "label": "Email", "actions": ["type"],
      "rect": { "x": 400, "y": 200, "width": 200, "height": 32 }, "value": "kid@example.com" },
    { "ref": "@e2", "role": "textbox", "label": "Password", "actions": ["type"],
      "rect": { "x": 400, "y": 240, "width": 200, "height": 32 }, "sensitive": true },
    { "ref": "@e3", "role": "button", "label": "Sign in", "actions": ["click"],
      "rect": { "x": 400, "y": 300, "width": 80, "height": 32 } }
  ],
  "total": 3,
  "truncated": false,
  "text": "@e1 textbox \"Email\" value=\"kid@example.com\"\n@e2 textbox \"Password\" [sensitive]\n@e3 button \"Sign in\"\n",
  "elapsed_ms": 61
}
```

- The current tab's interactive elements in reading order, the first `max_nodes` (1–1000, default 200). `truncated` when the page had more.
- `rect` is the page's viewport in CSS pixels, not screen pixels.
- A ref is good while this is the current tab's **latest** snapshot. A newer snap, a navigation, or a tab opened, switched or closed ends it.

### `POST /v1/browser/act`

```json
{ "ref": "@e1", "action": "type", "text": "kid@example.com", "allow_domains": ["shop.example"] }
{ "ref": "@e3", "action": "click", "snapshot": 48213 }
{ "ref": "@e3", "action": "click", "confirm": "9f2c…" }
```

- `action`: `click`, `type` (`text`), `select` (`text` is the option), `key` (`text` is the key: `Enter`, `Escape`, the arrows, `Home`, `End`, `PageUp`, `PageDown`), `scroll` (into view) or `focus`. `text` is up to 64 KiB, and only for those three.
- `ref` is `@eN` or `<snapshot>:@eN`. With `snapshot`, or the long form, a ref from anything but the latest snapshot is `409 stale_ref`.
- The page is looked at again first. The element is found by its identity, not its number, so one that moved is still hit, and one that's gone is `409 stale_ref`. If ultra-instinct's own look lands the press on another element, the answer is `409 stale_ref` too, and the press may have happened: snap again.

`200 {"ok": true, "ref": "@e3", "action": "click", "ticket": 41}`. Errors: `409 stale_ref`, `409 confirm_required`, `403 domain_not_allowed`, `422 action_refused` (ultra-instinct's guard said no: covered, disabled, drifted), `400 invalid_request`.

### `POST /v1/browser/navigate`

`{"url": "https://shop.example/", "allow_domains": [...]}` → `{"ok": true, "url": "https://shop.example/"}`. `http`, `https` or `about:blank`, up to 8 KiB.

### `POST /v1/browser/tabs`

`{"op": "list"}` → `{"op": "list", "tabs": [{"tab_id": "…", "url": "…", "title": "…"}]}`. `{"op": "open", "url": "…"}` → `{"op": "open", "tab_id": "…"}`, and that tab is the one every other call drives. `{"op": "switch" | "close", "tab_id": "…"}`.

### `POST /v1/browser/wait`

`{"text": "Welcome", "url": "/orders", "control": "Sign out", "gone": "Loading", "load": "domcontentloaded" | "load", "timeout_ms": 10000}`: every given condition, looked for about every 200 ms. With none, it waits out `timeout_ms` (1–60000, default 10000). → `{"seen": true, "after_ms": 840, "url": "…"}`.

### `POST /v1/browser/shot`

→ `{"format": "png", "data": "<base64>", "bytes": 48213, "width": 1280, "height": 720}`, the current tab's viewport. It's written nowhere.

### `POST /v1/browser/run`

```json
{ "goal": "Type \"rust\" into Search", "max_steps": 20, "timeout_ms": 120000, "allow_domains": ["docs.rs"] }
```

Instinct runs ultra-instinct's loop on the current tab: observe, choose or abstain, gate, ticket, execute, verify, until the goal is done, it's blocked, it abstains, or `max_steps` (1–50, default 20) or `timeout_ms` (1000–300000, default 120000) runs out. No model runs in the box: after an abstain the caller's model may take over, with snap and act.

```json
{ "outcome": "blocked", "reason": "needs-confirm", "confirm_label": "Place order", "confirm": "9f2c…",
  "steps": [ { "action": "type", "target": "Search", "effect": "success" } ], "elapsed_ms": 2310 }
```

`outcome` is `done`, `blocked` (`reason`, and `confirm_label` with a `confirm` token when a person may approve it: run again with `confirm`), `abstained` (`why`) or `failed` (`why`). `steps` are the verified ones, at most 50. Within a run the allow-list and the confirm globs are ultra-instinct's guard's to enforce: `box-exec` hands it both, but doesn't see a step before it happens.

### `POST /v1/browser/state`

`{"op": "save" | "load", "name": "default"}` → `{"op": "save", "name": "default", "cookies": 12, "origins": ["https://shop.example"]}`. A screen's logins: cookies, and the storage of the origin the tab is on, in `/home/box/browser-state/<screen>/<name>.json`, mode 0600, in a folder only `box-exec` can read. `<screen>` is the Bot holding the screen, or `main`. `name` is 1–64 letters, digits, `_` and `-`. Counts come back, never values. `load` refuses a file others could read; origins it couldn't write come back in `pending_origins`: navigate there and load again. `404 not_found` when no Bot holds the screen.

---

## box-host `:1340`

### `GET /v1/health` (public)

```json
{ "status": "ok" }
```

### `GET /v1/ready` (bearer)

`200` when `box-exec` answers `/v1/health`, and (when `BOX_DESKTOP_REQUIRED=1`) the X display is up. Otherwise `503` with `error.code = not_ready`. Chrome is not required for ready. Missing or invalid Bearer is `401`.

```json
{ "status": "ready", "service": "box-host", "exec_ready": true, "desktop_ready": true, "viewer_ready": true }
```

`viewer_ready` is whether the main screen's voxl takes connections. It never holds up ready: the box works unwatched. The X probe has a time limit, and a box without `xdpyinfo` is not ready when the desktop is required.

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
    "viewer": { "kind": "voxl", "port": 6901, "enabled": true, "ready": true },
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

Add and remove a Bot's screen while the computer runs with `POST /v1/screen` (bearer), `{"action": …, "bot": …}`. It answers with `box-screen`'s JSON, with an `error` when that failed:

- `up` starts it, and answers with its slot once its voxl takes connections.
- `down` stops it and keeps its slot, folder and profile.
- `forget` also ends the Bot's commands and frees its slot. Its folder, profile and users stay its own.
- `open` opens `url`, http or https, in its browser.
- `list` takes no `bot`.

Slots hold for the life of the home volume, so a Bot keeps its display number across restarts and updates. Every Bot has two users of its own, never given to another.

`?display=:N` puts any `/v1/cua/*` call on that screen. Through `/v1/cua` and the viewer, input on one screen does not reach another: each is its own X server with its own pointer, keyboard focus and clipboard.

Commands can't reach a screen at all, and a Bot's commands can't reach another Bot's folder: see [`USERS.md`](USERS.md), including what isn't a wall.

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
  "profile": "/home/box/desktops/main/chrome-profile",
  "display": ":1"
}
```

`ready` and `running` mean the process the entrypoint started is alive. There is no DevTools port: [`USERS.md`](USERS.md).

### `GET /v1/chrome/active-tab` (bearer)

The page in front of Chromium, as the box's own extension last reported it. `?display=:N` asks for a Bot's own screen; without it, the box's own.

```json
{ "url": "https://example.com/login", "atMs": 1794000000000, "focus": "password", "seq": 7 }
```

`url` is `null` (and the other fields are absent) when nothing has reported: no Chromium on that screen, or one started without the extension. `focus` is the kind of box focused on the page, as the page said: `password`, `text`, `other` or `none`. `seq` counts that browser's reports, so a reader that clicked a field can wait for it to move before it trusts `focus`. The report is dropped when Chromium closes its port.

A caller reads this before typing a saved login, and types nothing when the page in front is not the login's own site.

Trust limit. The report is a file in that screen's run folder (`/run/box/x/<slot>/run/active-tab.json`), written by `box-active-tab` as the screen's desktop user. A command (an agent user) cannot write it, and `box-host` refuses a file that is not that desktop user's own. But anything running as that desktop user, the browser included, could write it. It answers "which page is in front", not "prove no one lied", and is no weaker than the desktop user itself, who can already read the browser's profile. A box not run with separate users (`BOX_RUN_AS` not `agents`) has no owner check, and any process of the box user could write the file.

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
