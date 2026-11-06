# Terminology

Names used in this repository. The public product name, the workspace crate, and the CLI binary are all **buwiz-computer**.

| Term | Meaning |
| --- | --- |
| **buwiz-computer** | Guest image + `box-exec` / `box-host` + CLI + SDKs. Not published to crates.io / npm / PyPI. |
| **guest** | The Linux container running this image. Always Linux; Windows/macOS are clients or Docker hosts. |
| **box-exec** | HTTP daemon on 1337: exec, files, CUA. Our wire — not Cursor `/exec-daemon`. |
| **box-host** | HTTP daemon on 1340: health, ready, info, desktop, chrome. Our wire — not `sand-host`. |
| **BOX_TOKEN** | Bearer secret injected when the guest starts. Required. Wiped from daemon environ after load. Stripped from exec children. |
| **BOX_VIEWER_PASSWORD** | voxl's password, for every screen. Must not be derived from `BOX_TOKEN`. |
| **connect-only SDK** | Caller passes `execUrl`, `hostUrl`, and `token`. No `docker run` / `Sandbox.create()`. |
| **`/v1/info` endpoints** | Container-local listen URLs. Honest inventory, not the SDK connect source of truth. |
| **workspace jail** | `WORKSPACE_ROOT` (default `/workspace`). cwd and file APIs cannot leave it. |
| **CUA** | Computer Use against the X framebuffer. Coordinate space **1280×800**, origin **top-left**. |
| **recipe** | `POST /v1/cua/recipe`: many CUA steps in one request, lint-then-run, receipt. Not a reverse-web-mcp compiler. |
| **screenshot JSON** | `{ encoding, mime, width, height, bytes, png_base64 }` |
| **screenshot PNG** | Raw `image/png` body when `Accept: image/png` or `?format=png` |
| **L3** | Informal name for the guest (this image). Prefer “buwiz-computer guest”. |
| **L4** | Inference gateway. Not this repository. Do not implement it here. |
| **voxl** | Browser viewer, one per screen on `6900+N`. Password = `BOX_VIEWER_PASSWORD`. Not Bearer-authenticated by the box; host publish is loopback. |
| **Screen** | One agent's X display, input, viewer, browser profile and folder on a computer. `:1` is the box's own; `box-screen` adds a Bot's. |
| **CDP** | Chromium DevTools. The box opens no port for it: `docs/USERS.md`. |
| **egress tunnel** | `box-egress-tunnel`: Chromium CONNECT via a client on the operator laptop. Capability `egress_tunnel`. See [EGRESS.md](EGRESS.md). |
