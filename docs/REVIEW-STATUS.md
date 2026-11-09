# Review status (for follow-up agents)

**Repo:** `buwiz-bot/computer`, imported from `hexuria/box` at `64f2b13` with its history. `hexuria/box` is frozen; `buwiz-bot/computer` is the one name.  
**Baseline:** the slices below came from the original BOX-REVIEW P0 → P1 pass (selective P2), merged 2026-09-14 as `hexuria/box` PR #7. The review text itself was a conversation artifact and is not in this repo.  
**Since then:** [CHANGELOG.md](../CHANGELOG.md) at the repo root lists what changed, with the files; the egress review's findings, fixed and open, are in [review/findings-box30.md](review/findings-box30.md).

## Completed (original baseline)## Completed on `main`

### Slice A — contract freeze
- OpenAPI / README / API.md aligned with live routes
- SDK/CLI parity (Rust/TS/Python): rename, press/release, desktop/chrome, recipe fields
- Recipe `settle` defaults **off**; recipes do not launch/kill Chromium as side effects

### Slice B — honest image
- Single Chromium model via entrypoint (no fighting `chromium-launch.sh` in image)
- `reset_desktop` does not fight Chromium respawn

### Slice C — hardness + CI
- Compose: `cap_drop: ALL`, `no-new-privileges`, `pids_limit`, `mem_limit` (not a kernel sandbox — documented)
- `BOX_MAX_CONCURRENT_EXECS` → 429 `busy`; SIGTERM then SIGKILL; `exec_id`
- CI: `.github/workflows/ci.yml` (fmt, clippy `-D warnings`, tests, OpenAPI check, smoke-native)
- Rust client HTTPS (hyper-rustls); guest stays HTTP behind proxy

### Slice D / P1 — L2-facing
- `POST /v1/exec/stream` (NDJSON/SSE)
- Raw `GET`/`PUT /v1/files/raw`; listing truncation
- `GET /v1/desktop/windows`, `GET /v1/busy`, `POST /v1/shutdown`
- `x-request-id` echo
- GHCR publish workflow on `v*` tags (`.github/workflows/publish-image.yml`)

### Selective P2
- Detached exec + `GET /v1/exec/{id}`
- Lightweight `/v1/metrics`
- `BOX_DISPLAY_GEOM` coord coverage

## Explicitly deferred (do not treat as missing P0)

| Item | Reason |
| --- | --- |
| Interactive **PTY** (`pty: true`) | Returns 400; use `/v1/exec/stream` |
| Vendor **trycua/cua** | Optional later backend; Linux X11 is default |
| Docker image smoke on CI | `.github/workflows/image-smoke.yml` runs `scripts/smoke.sh` on PRs that touch `docker/**`, `docker-compose.yml` or the script. It is written, not yet seen to pass on a runner; run `scripts/smoke.sh` locally too. |
| amd64 image | Neither built nor tested; the publish workflow is written, not run |

## How to re-review

1. Check out the branch under review (`main` once merged), read `README.md` + `docs/ARCHITECTURE.md` + `docs/openapi.yaml`.
2. `cargo test --workspace` && clippy.
3. `./scripts/smoke-native.sh`; optionally `./scripts/smoke.sh` with Docker.
4. Compare OpenAPI to `crates/box-exec/src/routes.rs` + `crates/box-host/src/lib.rs`, with `bash scripts/check-openapi.sh`.
5. Regenerate the CLI reference with `bash scripts/gen-cli-docs.sh`; CI fails when `docs/CLI.md` is stale.
