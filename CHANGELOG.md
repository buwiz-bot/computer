# Changelog

## Unreleased

| Item | Where |
| --- | --- |
| `POST /v1/cua/tree` and `POST /v1/cua/element`: a screen's widgets as text over AT-SPI, Chromium's own included, and press, click, focus or set text on one by `ref`. Each screen has its own accessibility bus that only its desktop and `box-exec` can reach. In the Rust client and CLI (`cua tree`, `cua element`), both SDKs, the API doc, the OpenAPI spec and the image smoke | `crates/box-a11y`, `crates/box-exec`, `crates/box-cua`, `docker/box-a11y-bus`, `docker/a11y-bus.conf`, `crates/buwiz-computer`, `sdk/`, `scripts/smoke.sh` |
| `deploy/aws`: an OpenTofu module for what Buwiz Bot's EC2 computers launch into (launch template, network, SSH-only security group), and the broker's IAM policy with the account left as `<ACCOUNT_ID>`; `tofu output -raw broker_policy_json` fills it in for the account it runs in | `deploy/aws/`, `docs/DEPLOY.md` |
| Startup refuses a `BOX_TOKEN` or `BOX_VIEWER_PASSWORD` containing `replace-with` (the `.env.example` placeholders), even with `BOX_ALLOW_INSECURE_DEV=1` | `crates/box-common` |
| A malformed `BOX_*_BIND` (the egress binds included) or numeric `BOX_*` value is a startup error naming the variable; unset or empty means the default | `crates/box-common`, `crates/box-egress-tunnel`, `crates/box-host` |
| The Rust client and the CLI bound every request: `DEFAULT_TIMEOUT` (120 s) unless `BuwizComputer::with_timeout` or `--request-timeout-secs` / `BUWIZ_COMPUTER_REQUEST_TIMEOUT_SECS` says otherwise; an exec gets its `timeout_ms` plus `EXEC_MARGIN` (35 s), a wait its own plus `WAIT_MARGIN` (5 s); running out is `Error::Timeout` | `crates/buwiz-computer` |
| The ready check times out its X probe, and a missing `xdpyinfo` means not ready; no X tool the actions fall back on can hang a request | `crates/box-desktop`, `crates/box-host`, `crates/box-cua` |
| What a daemon reads from a peer on the box's loopback has a deadline and a size cap, and `probe_egress` no longer blocks an async handler | `crates/box-common/src/local_http.rs`, `crates/box-egress-tunnel`, `crates/box-host` |
| The egress client gives up dialling a destination after 10 s, before the guest stops waiting for it | `crates/box-egress-tunnel` |
| No secret written for Compose can end up in the Docker build context | `.dockerignore` |
| The image is built and smoke-tested when its Rust code changes, and again on `main` | `.github/workflows/image-smoke.yml` |
| Removed: the standalone Chromium launch script nothing ran. Each crate depends on what it uses, once, and no daemon can be published | `docker/`, `crates/*/Cargo.toml` |
| `hexuria/box` is frozen. `buwiz-bot/computer` is the one name for this repo, the image (`ghcr.io/buwiz-bot/computer`) and the docs | everywhere |
| Docs match the code: the Compose healthcheck is the public `/v1/health` with no token, `pids_limit` is 2048, `BOX_HOST_TOKEN` replaces `BOX_TOKEN` on every host route, the CLI shows the token as `BUWIZ_COMPUTER_TOKEN` and not in argv, the `connect(...)` examples handle its `Result`, token rotation goes through `scripts/write-secrets.sh`, the deploy doc clones into `computer`, the token compare says what it hides | `README.md`, `docs/`, `crates/box-common/src/auth.rs` |
| `docs/CLI.md`, generated from `--help` by `scripts/gen-cli-docs.sh`; CI fails when it is stale (`scripts/check-cli-docs.sh`) | `docs/CLI.md`, `scripts/`, `.github/workflows/ci.yml` |
| `GET /v1/chrome/active-tab` is in the API doc, the OpenAPI spec and both SDKs (`activeTab`, `active_tab`), with its trust limit | `docs/API.md`, `docs/openapi.yaml`, `sdk/` |
| Documented: `BOX_EGRESS_ALLOW_PRIVATE`, `BOX_EGRESS_RELAY_PORTS` and that the address filter applies to every relay; `BOX_SHARED_DISPLAY`, `BOX_SCREEN_WORKSPACE`, `BOX_VIEWER_CTL`, `BOX_EGRESS_BEARER_STAGED`, `BOX_MAX_OWN_SCREENS` and the exec size and timeout limits; the helpers `box-active-tab`, `box-refresh-extension` and `box-screen` | `README.md`, `docs/EGRESS.md`, `docs/ARCHITECTURE.md` |
| `docs/review/findings-box30.md` says which findings are fixed and which are open; `docs/REVIEW-STATUS.md` is current | `docs/review/`, `docs/REVIEW-STATUS.md` |
| The TypeScript SDK type-checks (`filesPutRaw`) | `sdk/typescript` |

## The Bot's computer

Imported from `hexuria/box` at `64f2b13` with its history, as `buwiz-bot/computer`.

| Item | Where |
| --- | --- |
| Named `buwiz-computer` throughout: crate, CLI, image, SDKs, `BUWIZ_COMPUTER_*`, native host `app.buwiz.active_tab` | everywhere |
| voxl 0.3.3 replaces x11vnc, websockify and noVNC: one per screen on `6900+N`, `BOX_VIEWER_PASSWORD` with `_FILE` | `docker/Dockerfile`, `docker/entrypoint.sh`, `docker/box-screen` |
| `box-keeper` is the one process that reads the secrets, and runs the rest. It, `box-exec`, `box-host`, `box-egress-tunnel` and voxl are undumpable, so an exec child cannot read their memory; `entrypoint.sh` holds no secret. Proved on every smoke by `scripts/memory-probe.py` | `crates/box-host/src/bin/box-keeper.rs`, `docker/` |
| A voxl that dies restarts after 1 s, doubling to 30 s; the fifo takes only allocated slots | `crates/box-host/src/keeper.rs` |
| `BOX_VIEWER_PASSWORD` gets the token strength check, with no insecure-dev exemption, before anything starts | `crates/box-common/src/config.rs` |
| `reset_desktop` on one screen leaves the others alone | `docker/box-reset-desktop.sh` |
| `/v1/info` `capabilities.viewer: {kind, port, enabled, ready}`; `/v1/desktop` `viewer.kind`, no more `vnc`; `/v1/ready` `viewer_ready` | `crates/box-host`, `crates/box-desktop` |
| `GET /v1/screens`: every screen's display, viewer port, state and folder | `crates/box-desktop`, `crates/box-host` |
| `POST /v1/cua/wait`, `/v1/cua/record/start`, `/v1/cua/record/stop` on voxl, password held by `box-exec` | `crates/box-cua/src/voxl.rs`, `crates/box-exec` |
| A folder per screen under `/workspace` (`agents/<bot>`, `shared`) for recordings, cooks and downloads | `docker/box-screen`, `crates/box-desktop` |
| `/v1/chrome` reports ready again: Chromium 156 refuses HTTP/1.0 on CDP. With `BOX_DESKTOP=0`, `capabilities.chrome.enabled` is false, as nothing starts it | `crates/box-chrome`, `crates/box-cua/src/settle.rs` |
| Everything the model runs is a user that can't reach a desktop, a browser or a service: each Bot has an agent user and a desktop user, `box-exec` and `box-host` are `boxsvc`, `box-keeper` alone is root. Per-screen X cookies, no `-ac` or `-shmem`, no terminal on the dock, a port floor at 6906. `POST /v1/screen`, `bot` on `/v1/exec` and `/v1/files*`. A volume from an older image keeps its files, with its home set aside | `docs/USERS.md`, `docker/`, `crates/box-exec`, `crates/box-host` |
| Chromium has no DevTools port: `/v1/chrome` reads its process, `BOX_CDP_PORT` and `cdp` are gone, and each screen's page comes from its own extension report | `crates/box-chrome`, `crates/box-cua`, `docker/entrypoint.sh` |
| Multi-arch publish to `ghcr.io/buwiz-bot/computer` on native runners, and an image smoke on PRs | `.github/workflows/` |
| Removed: the frozen `ensurebox/` and `l1/` demos, `docker/Dockerfile.remasure`, `box-release-keys` (voxl lets go of held keys when a viewer leaves) | history keeps them |

Compose's `pids_limit` is 2048. Measured per screen with Chromium: about 510 MiB and 180 processes and threads.

**Not done:**

- `settle` on actions: `/v1/cua/wait` covers it by hand.
- amd64 is neither built nor tested, and the publish and image smoke workflows are written, not run.
- A recording over `BOX_MAX_FILE_BYTES` (10 MB) may not come back through `/v1/files/raw`. Not tested.
- voxl letting go of held keys when a viewer leaves is read from its source, not tested live.
- Screens are not a security boundary: exec reaches every screen and folder.
- A secret on a read-only mount, Compose's included, stays readable to anything running as the box user. Only staging on a writable mount closes that. Moving secrets to another uid would need `CAP_SETUID` and a split between `box-exec` and its children.

**Breaking:** `BOX_VNC_PASSWORD` is now `BOX_VIEWER_PASSWORD`, with no fallback; `BOX_VNC_BIND`, `BOX_NOVNC_PORT` and port 6080 are gone; `/v1/desktop` lost `vnc`; the entrypoint is now `box-keeper`, so a start with `_FILE` set and no file there fails; Compose's healthcheck is the public `/v1/health`; default cook folder moved from `.l1/cooks` to `<screen folder>/cooks`.

Guest honesty and wire parity against BOX-REVIEW P0 → P1 (selective P2). Baseline `main` @ `3c67b576e2d4b67da5849b55bc2de6231d31ff47`.

## Egress tunnel (laptop CONNECT for prod Chromium)

| Item | Where |
| --- | --- |
| `box-egress-tunnel` crate: WS mux + HTTP CONNECT, fail-closed without client, local echo tests | `crates/box-egress-tunnel` |
| Guest starts the server before Chromium when `BOX_EGRESS_TUNNEL=1`; Chromium `--proxy-server` + bypass list | `docker/entrypoint.sh`, `docker/box-chromium`, `docker/Dockerfile` |
| Compose loopback publish **8790** only (never 8791/8792) | `docker-compose.yml` |
| `/v1/info` `capabilities.egress_tunnel: {enabled, ready}`; `GET /v1/egress` | `crates/box-host`, OpenAPI, CLI/SDKs |
| Docs: prod without host-network, client flags, capability JSON | `docs/EGRESS.md`, README, DEPLOY, ARCHITECTURE |

## P0

| Item | Where |
| --- | --- |
| Wire truth (press/release/aliases, recipe fields, rename, desktop/chrome) | `docs/openapi.yaml`, `docs/API.md`, `README.md`, Rust CLI, TS/Python SDKs |
| Capabilities `{enabled, ready}` | `crates/box-host` `/v1/info` |
| Chrome probe via CDP `/json/version` (no `/proc` scrape) | `crates/box-chrome` |
| Unified `env_bool` | `crates/box-common/src/env.rs` |
| Recipe `settle` default **off**; no Chromium launch/close side effects | `crates/box-cua` recipe/settle |
| One Chromium model (entrypoint); the standalone launch script is not in the image. `reset_desktop` does not kill Chromium | `docker/entrypoint.sh`, `docker/Dockerfile`, `docker/box-reset-desktop.sh` |
| Compose `cap_drop ALL`, `no-new-privileges`, `pids_limit`, `mem_limit` — not a kernel sandbox | `docker-compose.yml`, README |
| `BOX_MAX_CONCURRENT_EXECS` (default 8), 429 `busy`, SIGTERM then SIGKILL, `exec_id` | `crates/box-exec` |
| CI: test, clippy `-D warnings`, smoke-native, OpenAPI presence | `.github/workflows/ci.yml`, `scripts/check-openapi.sh` |
| Rust client HTTPS (hyper-rustls). Guest daemons remain HTTP; TLS is at a proxy | `crates/buwiz-computer`, `docs/DEPLOY.md` |

## P1

| Item | Where |
| --- | --- |
| `POST /v1/exec/stream` NDJSON / SSE | `crates/box-exec` |
| PTY deferred (`pty: true` → 400) | exec request + API.md |
| Raw file GET/PUT octet-stream; listing `truncated` cap | `/v1/files/raw`, `BOX_MAX_DIR_ENTRIES` |
| `GET /v1/desktop/windows` | `crates/box-desktop` + box-host |
| `GET /v1/busy`, `POST /v1/shutdown` | box-exec + box-host |
| GHCR publish on `v*` tags | `.github/workflows/publish-image.yml` |
| `x-request-id` echo | `crates/box-common/src/request_id.rs` |

## P2 (done)

| Item | Where |
| --- | --- |
| Optional detached exec + `GET /v1/exec/{id}` | box-exec |
| `GET /v1/metrics` | box-exec + box-host |
| `BOX_DISPLAY_GEOM` already drives CUA coords; added a regression test | `crates/box-cua` |
| Did **not** vendor trycua | — |

## Exec lifecycle and secret delivery (#12, #13, #14, #15)

| Item | Where |
| --- | --- |
| Background-spawning exec returns the foreground output instead of an empty body; new `output_complete`, and `truncated` is true whenever EOF was not observed (#13) | `crates/box-exec/src/exec.rs`, openapi, API.md, SDKs |
| Pipe readers are owned by the request, not detached tasks — no descriptor or task leak; `open_fds` on `/v1/metrics` (#14) | `crates/box-exec/src/exec.rs`, `src/fdcount.rs`, `/v1/metrics` |
| One `ChildGuard` owns the process group, so timeout, disconnect and explicit cancel all SIGTERM → SIGKILL the group; `DELETE /v1/exec/{id}`; `execs.child_groups` on `/v1/metrics` (#15) | `crates/box-exec`, openapi, API.md, SDKs, CLI |
| Secrets delivered as files (`BOX_TOKEN_FILE`, `BOX_HOST_TOKEN_FILE`, `BOX_VNC_PASSWORD_FILE`), read and unlinked; Compose and EnsureBox mount instead of exporting; `wipe_secret_environ` doc corrected to what `unsetenv` actually does (#12) | `crates/box-common/src/config.rs`, `docker/entrypoint.sh`, `docker-compose.yml`, `ensurebox`, README, ARCHITECTURE, DEPLOY |

## Recipe receipts report what was observed (#28)

| Item | Where |
| --- | --- |
| Optional `observe` on a recipe (`off` default / `input` / `page`); each step gains an `observed` block, and `ok` still means only that no step returned an error | `crates/box-cua/src/observe.rs`, `src/recipe.rs`, openapi, API.md, RECIPES.md, TS SDK, l1 |
| `target` — the window covering a pointer step's coordinate, read with `TranslateCoordinates` before the pointer moves, so a taped click that now lands elsewhere is visible | `crates/box-cua/src/x11.rs` |
| `focus` — where a `type` or `key` was about to go; `state: "none"` is text the X server discarded | `crates/box-cua/src/x11.rs` |
| `url_before` / `url_after` around click/type/key from the CDP HTTP endpoint, so a `Return` that submitted nothing is a URL that did not move | `crates/box-cua/src/observe.rs` |
| Observation has its own X connection and its own 400ms deadline, forks nothing, and never fails a step; `observe_ms` reports what each step spent looking | `crates/box-cua/src/x11.rs` |

## Deferred

- PTY exec (use `/v1/exec/stream`)
