# Changelog

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
| One Chromium model (entrypoint). `chromium-launch.sh` not in the image. `reset_desktop` does not kill Chromium | `docker/entrypoint.sh`, `docker/Dockerfile`, `docker/box-reset-desktop.sh` |
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
- Moving `ensurebox/` / `l1/` out of this repo (marked frozen demos / non-product)
