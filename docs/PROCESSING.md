# Request processing

How a call moves through buwiz-computer. Inference is out of scope (not this repo).

## Connect

The caller already has three values from **their** orchestrator:

1. `execUrl` — published `box-exec` base (`http://127.0.0.1:1337` under Compose)
2. `hostUrl` — published `box-host` base
3. `token` — the `BOX_TOKEN` they injected at container start

`BuwizComputer.connect(execUrl, hostUrl, token)` stores those. It does not start Docker. It does not replace them with `/v1/info.endpoints`.

## Unauthenticated probes

| Request | Process | Meaning |
| --- | --- | --- |
| `GET {execUrl}/v1/health` | box-exec | Process is alive (`{"status":"ok"}`) |
| `GET {hostUrl}/v1/health` | box-host | Process is alive (`{"status":"ok"}`) |

`GET {hostUrl}/v1/ready` requires Bearer. 200 when exec health OK, and X up when `BOX_DESKTOP_REQUIRED=1`.

Compose healthcheck is `/v1/ready` on 1340 with `Authorization: Bearer`. Chrome is not required for ready.

## Authenticated calls

All other routes require `Authorization: Bearer <token>`, including `/v1/ready`.

box-host uses `BOX_HOST_TOKEN` if set, otherwise `BOX_TOKEN`.

Failures use a single JSON envelope:

```json
{ "error": { "code": "unauthorized", "message": "missing or invalid bearer token", "status": 401 } }
```

CORS is not permissive. Server-to-server callers are unaffected. Browsers only get ACAO headers when `BOX_CORS_ORIGINS` lists their origin.

## Exec (`POST /v1/exec`)

1. Jail-check `cwd` under `WORKSPACE_ROOT`.
2. Spawn argv or `/bin/sh -c`.
3. Strip `BOX_TOKEN`, `BOX_HOST_TOKEN`, `BOX_VNC_PASSWORD`, and `BOX_EGRESS_TUNNEL_BEARER` from the child (including caller-supplied `env`). The daemons also drop those names from their own environ after config load.
4. Write `stdin` if provided, then close the pipe so the child sees EOF.
5. Cap stdout/stderr (`BOX_MAX_OUTPUT_BYTES`).
6. On timeout: **SIGTERM** the process group, wait `BOX_EXEC_KILL_GRACE_MS` (default 2s), then **SIGKILL**. Keep captured output. `timed_out: true`. Response includes `exec_id`.
7. Concurrent children are limited (`BOX_MAX_CONCURRENT_EXECS`, default 8). Overflow is **429** `busy`.

`POST /v1/exec/stream` is the incremental path (NDJSON or SSE). PTY is not implemented. Optional `detach: true` plus `GET /v1/exec/{id}` for fire-and-forget.

Every response echoes `x-request-id`.

## Files

| Method | Path | Behavior |
| --- | --- | --- |
| GET | `/v1/files?path=` | File content or directory listing |
| PUT | `/v1/files` | Write file (`create_dirs` default true) |
| DELETE | `/v1/files?path=` | Delete file or directory (`recursive=true` for trees) |
| POST | `/v1/files/mkdir` | Create directory (`parents` default true) |
| POST | `/v1/files/rename` | Rename a file or directory (`from`, `to`) |

Every path is jail-checked. The workspace root cannot be overwritten or deleted.

## CUA (`POST /v1/cua/*`)

Actuators talk to `DISPLAY=:1` (1280×800, origin top-left). Out-of-range coordinates → `400 out_of_range`.

| Verb | Notes |
| --- | --- |
| `screenshot` | JSON base64 PNG, or raw `image/png` (`Accept: image/png` or `?format=png`) |
| `click` | Move + button |
| `double-click` | Move + two clicks |
| `move` | Hover; no button |
| `drag` | Mouse down at (x1,y1), move to (x2,y2), mouse up |
| `type` | XTEST type (xdotool fallback) |
| `key` | X11 keysym |
| `scroll` | Move, then wheel repeats |
| **`recipe`** | **Many of the above in one request.** Lint the whole plan, then run sequentially. See [`RECIPES.md`](RECIPES.md). |

### Recipe (`POST /v1/cua/recipe`)

The caller already knows the choreography (click, type, key, …). One Bearer POST runs it. The pointer is shared, so steps are sequential. Invalid plans (`steps` empty, >256 steps, out-of-range coordinates) return **400** and do not move the mouse.

Default `screenshot` is `end` (one PNG on the receipt, and a file when `artifact_dir` is set). Use `none` if you will screenshot yourself. `each` is large. `record: true` starts ffmpeg/x11grab **before the first CUA step** and SIGINT-stops after the last (no `-t`; that capped tapes at 1s), then remuxes to `+faststart` so OS players and Chrome can play motion. `reset_desktop` closes windows on this display.

```json
{
  "name": "search",
  "stop_on_error": true,
  "screenshot": "end",
  "record": true,
  "artifact_dir": ".l1/cooks/demo",
  "steps": [
    { "op": "reset_desktop" },
    { "op": "click", "x": 640, "y": 80 },
    { "op": "type", "text": "hello" },
    { "op": "key", "key": "Return" },
    { "op": "wait", "ms": 200 }
  ]
}
```

## Host inventory

`GET /v1/info` (bearer) returns box id, capability flags, workspace path, and **container-local** endpoint URLs (`scope: container-local`). Use it to see whether desktop/chrome/CUA/egress_tunnel are up. Do not dial those URLs from another machine; dial the URLs you published.

`GET /v1/desktop`, `GET /v1/chrome`, and `GET /v1/egress` are the same: status inside the guest. Viewer and CDP addresses are loopback unless you published noVNC (6080) yourself. Egress `ws` is the guest listen; the laptop dials the URL **you** published or SSH-forwarded.

## Demo proxy (EnsureBox)

When the EnsureBox demo is running, a signed-in L1 user sends requests to `http://127.0.0.1:43142/api/v1/boxes/:id/...`. The L1 **server** attaches `ENSUREBOX_TOKEN`. The browser never receives that token or `BOX_TOKEN`. EnsureBox looks up the guest token and calls the TypeScript `buwiz-computer` SDK with the **published** host ports. Public box JSON omits VNC passwords and guest bind URLs.
