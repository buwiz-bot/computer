# deploy/e2b: the Buwiz Bot computer as an E2B template

E2B runs sandboxes from templates, not container images. This folder builds one template from the box
image (`ghcr.io/buwiz-bot/computer`) in **your** E2B team, with one command. Then the broker makes a
sandbox per computer from it.

```sh
E2B_API_KEY=<your team's key> bash deploy/e2b/build.sh
```

It needs only that key, Python 3.10 or newer, and network access. It prints the template's alias
(`buwiz-computer`) and its id. Paste the alias into Buwiz Bot as `BUWIZ_E2B_TEMPLATE`, with your
`E2B_API_KEY`. Running it again rebuilds the same alias. Nothing else is needed: `E2B_ACCESS_TOKEN` is
deprecated and not used ([docs](https://docs.e2b.dev/migration/access-token-deprecation.md)). The key
is read by the SDK from the environment and is never printed.

## What it builds

- `e2b.Dockerfile`: `FROM ghcr.io/buwiz-bot/computer:latest`, as `box`'s image, plus:
  - `/run/buwiz`, owned by `box`, mode 0700: the directory the broker's secrets are staged in.
  - `BOX_TOKEN_FILE=/run/buwiz/token` and `BOX_VIEWER_PASSWORD_FILE=/run/buwiz/viewer`, and the
    non-secret settings of `docker-compose.yml` (`BOX_DESKTOP`, `BOX_CHROME`, the binds on `0.0.0.0`
    inside the sandbox, `BOX_EGRESS_TUNNEL=0`, and so on). No secret is in the template, a build
    argument or the environment.
  - `TINI_SUBREAPER=1`. Outside Docker, tini is not PID 1, so it warns and does not reap orphans
    unless it is a subreaper. The variable does that without changing the start command.
  - `WORKDIR /workspace`.
- `template.py`: the start command, which runs once at build and is snapshotted with the sandbox:
  `sh -c 'until [ -f /run/buwiz/go ]; do sleep 0.2; done; exec /usr/bin/tini -- /usr/local/bin/box-keeper'`.
  It waits until the broker has uploaded `token`, `viewer` and then `go`, the trigger. box-keeper
  reads the two files and unlinks them.
- Resources: **8 vCPU and 8 GiB**, the most a Hobby team may give a sandbox
  ([billing](https://docs.e2b.dev/billing.md)). Pro allows more on request. Lower them with
  `BUWIZ_E2B_CPU` and `BUWIZ_E2B_MEMORY_MB` (even, at least 1024). Each sandbox is billed for what it
  runs, so the size is what a computer costs; the pricing FAQ is
  [calculate-sandbox-price](https://docs.e2b.dev/faq/calculate-sandbox-price.md) (not checked here).

## How it was built, and the docs it follows

- **Method: the E2B Python SDK** (`e2b==2.10.2`, in a venv under `~/.cache/buwiz-e2b`). The template
  is `Template().from_dockerfile(...)` with `set_start_cmd(...)`, then `Template.build(...)`.
  - [Quickstart](https://docs.e2b.dev/template/quickstart.md): the SDK and CLI ways of making a template.
  - [Base image and `fromDockerfile`](https://docs.e2b.dev/template/base-image.md): FROM, USER, RUN,
    ENV, WORKDIR convert; multi-stage Dockerfiles do not.
  - [Build](https://docs.e2b.dev/template/build.md): `Template.build(template, name, cpu_count=, memory_mb=)`.
  - [Start and ready commands](https://docs.e2b.dev/template/start-ready-command.md): the start command
    runs at the end of the build and is snapshotted.
  - [Template names](https://docs.e2b.dev/template/names.md): an alias is usable in place of an id.
- **Not used:** `e2b template create` with an `e2b.Dockerfile` (CLI reference
  [v2.23.0](https://docs.e2b.dev/sdk-reference/cli/v2.23.0/template.md)). It exists there, but the
  docs now describe `e2b template migrate` as converting `e2b.Dockerfile` and `e2b.toml` to the SDK
  format, so the SDK is the supported path. Docs also name no `e2b template build` command and no
  `e2b.toml` format, so neither is used.
- The Dockerfile's comments are left out on purpose: the SDK's reader prints one "Unsupported
  instruction" line for each. Keep ENV to one variable per line: the reader mangles a value that
  continues onto the next line (checked: the value gained trailing spaces).

## Checked here, and not

Checked, against the real image (`buwiz-computer:local`, built under a throwaway tag, then removed):
- `/usr/bin/tini` (0.19.0) and `/usr/local/bin/box-keeper` are where the start command and the
  broker's contract say; the image runs as `box` (uid 1000) and the broker's `e2b/mod.rs` names `box`.
- The image's ENTRYPOINT is `/usr/bin/tini -- /usr/local/bin/box-keeper`; the start command matches it.
- `e2b.Dockerfile` builds (`docker build` with a named build context over the local image).
- As `box`, `/run/buwiz` takes a file, so the broker's staging as `box` can write it.
- With `go` present, the start command runs tini and box-keeper, which then reads
  `BOX_TOKEN_FILE` as `/run/buwiz/token`, the path the broker writes.
- The SDK turns the Dockerfile into the template steps above (no network call).

**Not checked, and needs your E2B account:**
- The build itself. Nothing was built or sent to E2B.
- **The image tag.** `ghcr.io/buwiz-bot/computer` is published only on `v*` tags
  (`.github/workflows/publish-image.yml`). An anonymous `docker manifest inspect
  ghcr.io/buwiz-bot/computer:latest` returned `manifest unknown`, so there may be no `latest` yet. If
  the package is private, E2B needs registry credentials
  ([private registries](https://docs.e2b.dev/template/private-registries.md)); none are configured.
  To pin a release, change the `FROM` tag.
- The user the start command runs as. The Dockerfile sets `USER root` (the converted file keeps it),
  but the docs do not say which user a start command runs as.
- That E2B's envd writes the staged files as `box` in `/run/buwiz`, and the box starts after `go`.
  Only the path and the user were checked locally, not E2B's upload.
- Region. The billing page lists no regions, and an EU shared cluster exists
  ([EU](https://docs.e2b.dev/faq/eu-region.md)). Asia is not confirmed.

## Limits to know

- A running sandbox lasts **1 hour on Hobby and 24 hours on Pro** of continuous runtime, then pauses
  itself ([billing](https://docs.e2b.dev/billing.md)). The broker keeps the memory (pause, not kill).
- Hobby: 20 concurrent sandboxes, 20 concurrent template builds, 1 sandbox creation a second, 10 GiB disk.
- The template's build limit is 1 hour ([build](https://docs.e2b.dev/template/build.md)).
