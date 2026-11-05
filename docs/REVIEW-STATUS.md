# Review status (for follow-up agents)

**Branch:** `main`  
**Baseline merge:** [PR #7](https://github.com/buwiz-bot/computer/pull/7) — BOX-REVIEW P0 → P1 (selective P2)  
**Merged:** 2026-09-14 (commit includes merge of `gol/box-review-p0-p1-cff5`)

Prior analysis-only review: conversation artifact `BOX-REVIEW.md` (keep/cut/add). Implementation tracked in PR #7 body + `docs/CHANGELOG.md` if present.

## Completed on `main`

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
| Full Docker image smoke on CI runner | May be environment-limited; use `scripts/smoke.sh` locally |

## How to re-review

1. Checkout `main`, read `README.md` + `docs/ARCHITECTURE.md` + `docs/openapi.yaml`.
2. `cargo test --workspace` && clippy.
3. `./scripts/smoke-native.sh`; optionally `./scripts/smoke.sh` with Docker.
4. Compare OpenAPI to `crates/box-exec/src/routes.rs` + `box-host` routes.
