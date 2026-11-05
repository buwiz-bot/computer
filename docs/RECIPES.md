# Recipes (one call, many CUA steps)

This is buwiz-computer’s custom take on [reverse-web-mcp](https://github.com/hexuria/reverse-web-mcp): **pay the model once to write a plan, then execute that plan as one guest request.**

It is **not** a fork of `rwmcp`. buwiz-computer is the Linux **screen** (Xvfb + XTEST). reverse-web-mcp is a **compiler** for apps that publish a world model (`x-reverse-webmcp` on OpenAPI). Different layer, same idea: stop doing click → screenshot → click as N tool calls.

## What reverse-web-mcp does

- The app declares what operations *mean* (postcondition, read/write footprint, surfaces).
- A goal becomes **wants** (predicates). A compiler turns wants into a **DAG**.
- A **scheduler** runs the DAG as wide as the data allows. API effects can run in parallel. UI-only ops need a screen surface (`crates/driver`, CDP).
- A successful plan is saved as a **recipe** with `$placeholders`. Re-runs cost **zero** model calls.
- A **receipt** proves what ran (idempotency keys, no double-sends).

The measured win vs a WebMCP/MCP loop is model calls and tokens, not pixels.

## What buwiz-computer CUA already was

Each verb is its own HTTP round trip:

`POST /v1/cua/click` then `POST /v1/cua/type` then `POST /v1/cua/key` …

That is the “agent does click/scroll on multiple requests” path. Fine when the **next** action depends on a new screenshot. Expensive when the path is already known (open URL bar, type, Return).

There was **no** preceding batch/recipe endpoint on this guest. Single-op CUA (including press / release / double-click / drag / move) is still there.

## What buwiz-computer recipes are

`POST /v1/cua/recipe` — one Bearer call, many sequential pointer steps, one **receipt**.

```json
{
  "name": "focus-and-search",
  "stop_on_error": true,
  "screenshot": "end",
  "steps": [
    { "op": "click", "x": 640, "y": 80, "button": 1 },
    { "op": "type", "text": "https://example.com" },
    { "op": "key", "key": "Return" },
    { "op": "wait", "ms": 400 }
  ]
}
```

The guest **lints the whole plan first** (empty, >256 steps, out-of-range coordinates, bad keys). If lint fails, **nothing** moves. Then it runs in order. Default screenshot is `end` (one PNG on the receipt, and a file under `artifact_dir` when that field is set). `none` skips it. `each` attaches a PNG after every non-wait step (heavy). `settle: "raw"` waits until Chromium is mapped and the title/URL changes after Return; `compressed` uses shorter timeouts. Default is **`off`** (no extra Chromium waits and no launch/close side effects). Teach `wait` steps are always honored.

`record: true` starts ffmpeg/x11grab on the same X display **before the first CUA step** (before `warmup_pointer`) and sends SIGINT after the last step so the MP4 is finalized. There is no `-t` duration cap (that is what made tapes stop at 1s). Live capture is fragmented so SIGINT can finish the file; a second pass remuxes to progressive `+faststart`. Finder, QuickTime, and Chrome play `frag_keyframe+empty_moov` x264 as a still (Play does not change pixels). Exact commands:

```bash
ffmpeg -nostdin -hide_banner -loglevel error -y \
  -f x11grab -draw_mouse 1 -video_size 1280x800 -framerate 8 -i :1.0 \
  -an -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
  -b:v 700k -maxrate 900k -bufsize 1800k \
  -movflags +frag_keyframe+empty_moov+default_base_moof \
  /workspace/.l1/cooks/<run>/cook.mp4

ffmpeg -nostdin -hide_banner -loglevel error -y \
  -i /workspace/.l1/cooks/<run>/cook.mp4 \
  -an -c:v copy -movflags +faststart \
  /workspace/.l1/cooks/<run>/cook.faststart.mp4
```

The video is a guest file (`cook.mp4`), not a new VNC session. `reset_desktop` (alias `reset`) closes non-Chromium guest windows — **same box id**, no Docker restart. Chromium stays with the entrypoint (killing it would fight the respawn loop).

Wait is capped at 10s per step. `stop_on_error` defaults true; earlier steps stay in the receipt (`ok: false`, `stopped_at`).

## A receipt says no step errored, not that anything happened

A taped recipe is a list of fixed coordinates. Replay it against a desktop that has moved on and every step is still *delivered*: the click lands somewhere, the text goes to whatever holds focus, `Return` is pressed. Nothing raises an error, so `ok` is `true` and `stopped_at` is `null` — and that receipt is identical to the receipt of the run that worked.

That is what `ok` has always measured, and it stays that way. Making it cleverer would mean the guest guessing at what a recipe was *for*, and a wrong guess in the confident direction is the failure being fixed here. The guest reports what it **saw** instead, and the caller judges.

`observe` is `off` (default), `input`, or `page`:

```json
{
  "observe": "page",
  "steps": [
    { "op": "click", "x": 529, "y": 126 },
    { "op": "type", "text": "kabisado" },
    { "op": "key", "key": "Return" }
  ]
}
```

Each step that had something to look at gains an `observed` block:

```json
{
  "index": 0, "op": "click", "ok": true, "ms": 14,
  "observed": {
    "target": { "id": "0x02a00003", "class": "chromium.Chromium", "title": "kabisado - YouTube" },
    "url_before": "https://www.youtube.com/results?search_query=kabisado",
    "url_after": "https://www.youtube.com/results?search_query=kabisado",
    "observe_ms": 3
  }
}
```

Three facts, no verdict:

- **`target`** — the window covering the step's target coordinate, read with `TranslateCoordinates` *before* the pointer moves, so it is what the click is about to hit rather than what the click left behind. A taped click that now lands on a different window shows up as a `class` or `title` that changed between runs.
- **`focus`** — where the keys were about to go, for `type` and `key`. `state: "none"` means no window held the keyboard focus and the X server discarded the keystrokes.
- **`url_before` / `url_after`** — the page Chromium was showing either side of a `click`, `double_click`, `type` or `key`. A `Return` that submitted nothing is a URL that did not move.

Absence is load-bearing. **No `observed` key on a step means the guest did not look.** A block that is present with a field missing means it looked and got no answer — a window that closed mid-observation, an X server that did not answer inside 400 ms, a Chromium that is not running. Neither is ever reported as an empty string, because "the guest saw nothing there" and "the guest could not see" are different facts. The receipt also echoes `observe` at the top level (absent when `off`), so a receipt read a long way from its request still says whether the guest was looking at all.

What it costs:

| mode | per step | mechanism |
| --- | --- | --- |
| `off` | nothing | no probes; the receipt is byte-identical to before the field existed |
| `input` | ~10 X round-trips for a pointer step, ~4 for a `type` | `TranslateCoordinates` down to the window at the coordinate, `QueryTree` + `GetProperty` up to the client window that names it, `GetInputFocus` |
| `page` | `input`, plus **two** DevTools calls (one each side) on click / double\_click / type / key | Chromium DevTools `GET /json` on loopback |

Put beside what a step already costs: `CLICK_GAP` is 12 ms per click and typed characters are paced at 30 ms each, so observing a click is an order of magnitude cheaper than clicking it. `page` is the mode that scales into something worth noticing — 256 steps is up to 512 DevTools calls — which is why it is separate from `input` rather than bundled into one switch.

No process is forked for either mode. This deliberately does not go through `wmctrl` or `xdotool`, which the settle path uses and which cost a fork, an exec and a fresh X connection every time. Observation runs on its own X connection so a slow read cannot park the socket the next keystroke needs; each X observation is bounded at 400 ms and each DevTools read at 250 ms, so neither a wedged server nor a hung browser can stretch a recipe. A probe that fails costs the receipt a fact, never the recipe a step. `observe_ms` on each block reports what that step actually spent looking, and is not counted in the step's `ms` — so the cost is measurable from a receipt rather than taken on trust.

`observe` never waits for a page to load and never launches anything — that is what `settle` is for. `url_after` is read as soon as the step returns (after any `settle` wait), so a `Return` that *did* navigate will often still show the old URL there and the new one in the next step's `url_before`.

CLI:

```bash
cargo run -p buwiz-computer -- cua recipe --file plan.json
```

SDKs: `client.recipe({ steps: [...] })`. EnsureBox demo proxy: `POST /api/v1/boxes/:id/cua/recipe`.

## Honest comparison

| | reverse-web-mcp | buwiz-computer single CUA | buwiz-computer recipe |
| --- | --- | --- | --- |
| Who writes the plan | Compiler from wants + world model | The model, every turn | The caller (usually the model, once) |
| Model in the loop | Median 1, then 0 on `--recipe` | Every click | Not during this HTTP call |
| Parallelism | DAG of API effects | n/a | **No** — one X pointer |
| Receipt | Ledger, idempotency keys | `{ok:true}` per verb | Per-step `ok` / `ms` / optional PNG / optional `observed` |
| Screen | Optional CDP driver | X11 1280×800 | Same X11, batched |
| WebMCP | Consumes app-published actions | Not in this repo | Not in this repo |
| When to use | Annotated business app | Next click needs a new picture | Choreography already known |

CUA cannot be a parallel DAG. Two clicks share one cursor. Recipes still win on **round trips** and **model turns** when the path does not need vision between steps.

Optional `record: true` plus `artifact_dir` writes a cook video and PNG files so a UI can show them without stuffing megabytes of base64 through the receipt. L1’s **Record cook** switch (default on) uses that. `reset_desktop` is a real step for a clean empty desktop between runs.

If the agent must look at the framebuffer after every click, keep single-op CUA (or `screenshot: "each"`, which is still one HTTP call but a large body).

## What this is not (yet)

- Not a world-model compiler. The guest does not parse wants like `invoice(customer=…).status='sent'`.
- Not WebMCP. Chromium in the guest may load a site that speaks WebMCP; buwiz-computer does not broker those tools yet.
- Not mixed exec/files + CUA in one document. Exec and files stay their own routes. A later `/v1/recipe` could mix them; CUA stays sequential even then.

The product stay: you start the guest, `connect(execUrl, hostUrl, token)`, send a recipe.
