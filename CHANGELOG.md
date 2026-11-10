# Changelog

## Unreleased

| Item | Where |
| --- | --- |
| `/v1/browser/*` is on with `BOX_BROWSER` and `BOX_DESKTOP`, whatever `BOX_CHROME` says (`box_common::browser_layer_on`): `BOX_CHROME=0`, as Buwiz Bot's broker makes a computer, still starts no Chromium on the main screen and keeps that screen closed to the layer (`503 browser_disabled`, as before, even for a Chromium a person opens there from the dock), so only a Bot's own screen, whose browser `box-screen open` starts on a pipe, can be driven by its controls. `GET /v1/info` says so in `capabilities.browser {enabled, ready}`, `ready` meaning only that the desktop is up; an image without it serves the routes only with `BOX_CHROME` on. The fixture shop has a fake sign-in page (`/login`), and the image smoke recreates the box with `BOX_CHROME=0` and fills it on a Bot's own screen as the broker will: the front tab, a snap under the site's `allow_domains`, a type per field by ref (the password only into a sensitive field), Enter with no confirm, the typed password in no answer, an off-site type refused, a saved session, and the main screen still `browser_disabled` | `crates/box-common`, `crates/box-browser`, `crates/box-host`, `README.md`, `docs/API.md`, `docs/ARCHITECTURE.md`, `docs/openapi.yaml`, `scripts/fixtures/browser-shop.pl`, `scripts/smoke.sh` |
| Rust 1.99.0 everywhere (toolchain, MSRV, the image's builder, CI), clean under its lints. The lock moved: uuid 1.26.1 → 1.29.2, zbus 5.13.2 → 5.14.0, zvariant 5.9.2 → 5.15.1 (atspi 0.30 holds zbus below 5.15) | `rust-toolchain.toml`, `Cargo.toml`, `docker/Dockerfile`, `.github/workflows/ci.yml` |
| Each screen's Chromium runs on a DevTools pipe, through `box-chrome-pipe`, whose other end only `box-exec` gets: offered once in the screen's run folder, to a `boxsvc` peer, still with no port | `crates/box-chrome`, `docker/entrypoint.sh`, `docker/box-chromium`, `docs/USERS.md`, `docs/ARCHITECTURE.md` |
| `/v1/browser/*` (snap, act, navigate, tabs, wait, shot, run, state) through ultra-instinct, with snapshot-scoped `@eN` refs, no password text, confirm tokens for `Pay*`-like clicks, per-call allow-lists and saved logins at 0600. On ultra-instinct's pipe server (main 22885b3): presses bound to the state they were read in (`409 stale_ref` when it moved), a goal run (`run`) under the call's allow-list and confirm rules, `sensitive` from ultra-instinct (failing closed), typed secrets masked in every answer and hidden from a shot, a tool allow-list, and a frame too large costing only its call. In the Rust client and CLI (`browser …`), both SDKs, the API doc, the OpenAPI spec and the image smoke | `crates/box-browser`, `crates/box-exec`, `crates/buwiz-computer`, `sdk/`, `docs/`, `scripts/smoke.sh` |
| Chromium has a process group of its own, which its launcher kills 3 s after a signal it passes on, and when Chromium exits; `box-exec` sends `Browser.close` to every screen's Chromium as it stops, and never drops a live pipe by accident (Chromium quits with status 0 when its pipe closes) | `crates/box-chrome`, `crates/box-browser`, `crates/box-exec`, `docs/ARCHITECTURE.md` |
| A key press asks for the confirm of what it would press: the guard checks an Enter or a Space in a form against that form's submit button, in `act` and in a goal run alike, and `confirm` is taken on `key` and `type` as well as `click`. A blocked run says what stopped it (`code`, `host`); uploads are off in the server too | `crates/box-browser`, `docs/API.md`, `docs/openapi.yaml`, both SDKs, `scripts/smoke.sh` |
| A browser press can't go wrong quietly: a ref and a `snapshot` naming different snapshots are refused, a confirm is spent only once its press goes out, a call whose caller stopped waiting isn't run, and no press is sent with too little time left to answer (a `504 browser_timeout` says a press already sent may still land). `shot` may be a JPEG, as `format` says, in the spec and the SDKs, and `browser shot -o` names the file by it | `crates/box-browser`, `crates/buwiz-computer`, `docs/`, `sdk/typescript` |
| A label is checked and confirmed whole, and only clipped where it's shown, so a long one can be confirmed. `snap` and `wait` refuse a body that isn't JSON (`400 invalid_request`) instead of dropping it. The TypeScript SDK waits 60 s for a browser call, past box-exec's 45 s budget. The docs say a page script's own submit on a `select` or an unnamed click isn't asked about | `crates/box-browser`, `crates/box-exec`, `sdk/typescript`, `docs/API.md` |
| A press whose element now shows another label than the snap did is `409 stale_ref`, not a press on whatever took its place. With `allow_domains` set, only `about:blank` of the pages with no host gets input (`file:` and `data:` don't). Chromium dies with its launcher, even when the launcher is killed outright, and a signal before Chromium is up no longer leaves it behind. The saved-login owner lookup runs off the async threads | `crates/box-browser`, `crates/box-chrome`, `crates/box-exec` |
| One copy of each rule that was copied: the main screen's slot and the user who must own a screen's sockets (`box_desktop::main_slot`, `screen_owner`), which the X, accessibility-bus and browser-pipe checks and the saved-login owner now share. An empty `BOX_SHARED_DISPLAY` is `:1`; one that isn't a display name now refuses those checks and `/v1/browser/state` instead of taking slot 1, and a display number is digits only. A `BOX_DISPLAY` that isn't a display name is a startup error of box-keeper's, before it reads a secret (unset or empty is `:1`), where it took slot 1, and the screen list names no box screen for one. A socket's peer user is `box_common::peer::peer_uid`. The shell scripts' on/off rule is one sourced file, `/usr/lib/box/flag.sh`, which trims as `env_bool` does. A browser answer is measured against its cap without being written out | `crates/box-desktop`, `crates/box-common`, `crates/box-chrome`, `crates/box-browser`, `crates/box-a11y`, `crates/box-exec`, `docker/box-flag.sh`, `docker/entrypoint.sh`, `docker/box-chromium`, `docker/Dockerfile` |
| The allow-list reads a host only from an `http:` or `https:` address: any other scheme (`data:`, `blob:`, `view-source:`, `file:`, …) is off a list whatever its text holds, so `data:text/html,https://shop.example/` no longer passes for shop.example | `crates/box-browser`, `docs/API.md` |
| `snap` takes `"values": true` (`browser snap --values`, `values` in both SDKs): elements may then carry `value` (a field's text when its input type is known and isn't password or hidden, or a select's chosen option; 400 characters, `value_truncated` when cut), `checked` and `expanded`, and `text` shows them. A sensitive element never carries any of them, checked in box-browser as well as ultra-instinct. Off, as by default, a snap is as before; a press's own look never asks. The API doc no longer says both that a value never comes and that one does | `crates/box-browser`, `crates/buwiz-computer`, `sdk/`, `docs/API.md`, `docs/openapi.yaml`, `docs/CLI.md` |
| CI checks the SDKs in a job of their own: the TypeScript SDK typechecks with TypeScript 5.6.3, pinned with its lockfile (`npm run typecheck`, on Node 18), and the Python SDK compiles and imports on Python 3.10, its minimum | `.github/workflows/ci.yml`, `sdk/typescript/package.json`, `sdk/typescript/package-lock.json` |
| `values` on a snap needs `allow_domains` (400 without) and shows none of what an off-list page's fields hold; a `combobox` shows no text (it may be typed in); the docs say what `values` can show of an unlabelled secret | `crates/box-browser`, `crates/buwiz-computer`, `docs/`, `sdk/` |
| An error may carry `error.details` (`confirm_required`'s label and token, `domain_not_allowed`'s host) | `crates/box-common` |
| Compose keeps all of `/home/box` on a named volume, `box-home`: every screen's browser profile, saved logins, and the Bots' slots and users outlive a recreated container. `./chrome-profile` is no longer mounted (DEPLOY.md says how to carry it over), and `docker compose down -v` deletes the volume. The image smoke runs in a project of its own on a clean home, checks that a saved login loads, both profiles stay and a Bot keeps its slot and uid across a recreate, and deletes that volume when done. `scripts/smoke.sh`, `run-local.sh` and `smoke-native.sh` are executable, as the docs run them | `docker-compose.yml`, `scripts/`, `README.md`, `docs/DEPLOY.md`, `docs/USERS.md`, `docs/ARCHITECTURE.md` |
| ultra-instinct moves from main 22885b3 to 0ff09c4. A browser `act` click (and a goal run's) is now a real pointer press at the element's centre, not a script click: the page sees trusted pointer and mouse events and focus moves, and an element covered at its centre is refused `422 action_refused` rather than clicked through. A goal run under `allow_domains` is refused `403 domain_not_allowed` when the page it starts on is off the list; box-browser still refuses every `blob:` page under a list, where ultra-instinct's own list now lets one through by its origin. No tool or error code changed. The lock moved only the twelve ultra-instinct crates, no other | `Cargo.toml`, `Cargo.lock`, `crates/box-browser`, `docs/API.md`, `docs/ARCHITECTURE.md`, `docs/openapi.yaml` |
| ultra-instinct moves from main 0ff09c4 to 6507ebd. Under `allow_domains` ultra-instinct's guard now refuses every `blob:` page too (box-browser turns its `refuse_blob` on), so a page a goal run reaches mid-run is held to the same list as an `act`: the run ends `blocked`, `domain_not_allowed`, nothing pressed. A refused page with no host is named by its scheme in `error.details.host` (`a blob: page`, `a data: page`, `an https: URL with no host`, `(unknown url)`), where box-browser said `blob: (no host)`. A snap with `values` adds `selected` (`aria-selected`) and `current` (`aria-current`), under the same rules as `checked`. Labels drop zero-width characters at the source, and box-browser compares labels as ultra-instinct does, so one can neither slip past a confirm nor spoil its token. No error code changed. The lock moved only the twelve ultra-instinct crates; box-browser now names `ultra-instinct-core` too | `Cargo.toml`, `Cargo.lock`, `crates/box-browser`, `crates/buwiz-computer`, `docs/`, `sdk/` |
| A browser whose pipe `box-exec` let go of while it still ran is started again, not left closed (the one way that happens: a Chromium that stopped reading long enough for ultra-instinct to cut a request off mid-write at its 20 s deadline, after which it holds the pipe closed): `box-exec` keeps the connection the pipe came over as a lease, and says "let go" on it before it drops a pipe; `box-chrome-pipe` counts that only while Chromium is alive with its pipe open, and then exits 75 when Chromium exits 0, which the entrypoint's restart loop and now `box-chromium` (a Bot's screen, which passes TERM, INT and HUP on to the launcher) start Chromium again on, on `about:blank`. A person closing the browser, even with a call in flight, and `box-exec`'s shutdown, still leave it closed | `crates/box-chrome`, `crates/box-browser`, `docker/entrypoint.sh`, `docker/box-chromium`, `docs/ARCHITECTURE.md`, `docs/USERS.md`, `docs/API.md`, `scripts/smoke.sh` |
| The docs say who can take a screen's DevTools pipe: any `boxsvc` process, `box-exec` or `box-host`, not `box-exec` by name. The launcher checks a user, and can't check a program: another user's `/proc/<pid>/exe` is closed to it. A user of `box-exec`'s own would close that; it isn't done | `docs/USERS.md`, `docs/ARCHITECTURE.md`, `crates/box-chrome`, `docker/entrypoint.sh` |
| A browser press that landed on another element than its ref named is `409 pressed_other` (with `error.details.label` when known), not `409 stale_ref`: something was pressed, and the caller snaps before anything else. `stale_ref` keeps meaning nothing was pressed | `crates/box-browser`, `crates/box-exec`, `crates/buwiz-computer`, `docs/API.md`, `docs/openapi.yaml`, `sdk/` |
| `BOX_BROWSER_CONFIRM_PRESS` defaults to `Pay*,Delete*,Buy*,Place order*,Checkout*,Submit order*,Confirm purchase*,Remove*,Order*,Purchase*,Subscribe*`: a press on a checkout, an order, a purchase, a removal or a subscription asks a person too. A label that only starts like one ("Order history", "Remove filter") is asked about as well | `crates/box-browser`, `README.md`, `docs/API.md`, `docs/openapi.yaml` |
| ultra-instinct moves from main 6507ebd to eb32599, wording only: a hostless page is "an about: page" where it said "a about: page", and a goal run blocked on a page with no host now names it in `host` (`a blob: page`) where it had none. box-browser's own wording follows. The lock moved only the twelve ultra-instinct crates | `Cargo.toml`, `Cargo.lock`, `crates/box-browser`, `docs/API.md`, `docs/ARCHITECTURE.md` |
| ultra-instinct moves from main 84362cb to a3e90d0. Whether a refused address names a host is asked of its `refused_host`, not read from the shape of its wording; its new stable code `not_owned` (an attached browser's, never met on a pipe) reads as the browser's failure | `Cargo.toml`, `crates/box-browser` |
| ultra-instinct moves from main eb32599 to 84362cb. A scheme-less `host:port` refused under `allow_domains` (`shop.example:443/cart`, `localhost:8080`) is named by its host in `error.details.host` and in a blocked run's `host` (`shop.example`), where box-browser said `(unknown url)`; it is still off every list. A scheme box-browser doesn't know (`tel:`, `mailto:`) is still `(unknown url)`, and `ftp://host/` still names its scheme. ultra-instinct's pipe browser is now `CdpBrowser` (its `PipeBrowser` name an alias), its I/O moved behind a link that also serves its `aui open --attach`; box-browser uses the new name, and its tests now hold the pipe to the frame cap, the call deadline, the cut-off that closes it, the non-blocking writer and the close on a drop. No tool or error code changed. The lock moved only the twelve ultra-instinct crates | `Cargo.toml`, `Cargo.lock`, `crates/box-browser`, `docs/API.md`, `docs/ARCHITECTURE.md` |

## 0.1.0 (2026-11-09)

| Item | Where |
| --- | --- |
| `POST /v1/cua/tree` and `POST /v1/cua/element`: a screen's widgets as text over AT-SPI, Chromium's own included, and press, click, focus or set text on one by `ref`. Each screen has its own accessibility bus that only its desktop and `box-exec` can reach. In the Rust client and CLI (`cua tree`, `cua element`), both SDKs, the API doc, the OpenAPI spec and the image smoke | `crates/box-a11y`, `crates/box-exec`, `crates/box-cua`, `docker/box-a11y-bus`, `docker/a11y-bus.conf`, `crates/buwiz-computer`, `sdk/`, `scripts/smoke.sh` |
| `deploy/aws`: an OpenTofu module for what Buwiz Bot's EC2 computers launch into (launch template, network, SSH-only security group), and the broker's IAM policy with the account left as `<ACCOUNT_ID>`; `tofu output -raw broker_policy_json` fills it in for the account it runs in | `deploy/aws/`, `docs/DEPLOY.md` |
| `deploy/aws/cloudformation/buwiz-computer.yaml`: the same AWS setup as the OpenTofu module, for the console (VPC, SSH-only security group, launch template `buwiz-computer`, optional role, the broker's policy and user, no access key). `publish.sh` uploads it to a bucket you own and prints the quick-create link. `deploy/aws/check-parity.sh` fails when the two drift | `deploy/aws/cloudformation/`, `deploy/aws/check-parity.sh`, `deploy/aws/README.md`, `docs/DEPLOY.md` |
| `deploy/e2b`: the box as an E2B template, built in the client's own team with `E2B_API_KEY=… bash deploy/e2b/build.sh` (E2B's Python SDK). It prepares `/run/buwiz` for the broker's staged secrets and starts box-keeper when `go` is staged | `deploy/e2b/`, `docs/DEPLOY.md` |
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
