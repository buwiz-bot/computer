# Startup

How to bring buwiz-computer up and talk to it. The product is the guest image plus HTTP, CLI, and SDKs. EnsureBox and L1 are optional demos.

## 1. Guest (required)

From the repository root:

```bash
cp .env.example .env          # set BOX_TOKEN and BOX_VNC_PASSWORD
docker compose up --build
```

Wait until host ready is 200 (Bearer required):

```bash
curl -fsS -H "Authorization: Bearer $BOX_TOKEN" http://127.0.0.1:1340/v1/ready
```

Compose healthcheck uses the same path with the container env token.

You now have:

| URL | Service |
| --- | --- |
| `http://127.0.0.1:1337` | box-exec |
| `http://127.0.0.1:1340` | box-host |
| `http://127.0.0.1:6080/vnc.html` | noVNC (loopback publish; not Bearer) |

Token: `BOX_TOKEN` from `.env` (required). VNC password: `BOX_VNC_PASSWORD` (required when desktop is on; x11vnc uses 8 chars).

### Talk HTTP

```bash
curl -fsS http://127.0.0.1:1337/v1/exec \
  -H "Authorization: Bearer $BOX_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"command":["echo","ok"]}'
```

See [API.md](API.md) for files and CUA.

### CLI

```bash
cargo run -p buwiz-computer -- \
  --exec-url http://127.0.0.1:1337 \
  --host-url http://127.0.0.1:1340 \
  --token "$BOX_TOKEN" \
  ready

cargo run -p buwiz-computer -- \
  --exec-url http://127.0.0.1:1337 \
  --host-url http://127.0.0.1:1340 \
  --token "$BOX_TOKEN" \
  exec -- echo ok
```

Environment aliases: `BUWIZ_COMPUTER_EXEC_URL`, `BUWIZ_COMPUTER_HOST_URL`, `BUWIZ_COMPUTER_TOKEN`.

### SDKs

Connect-only. Your process already knows the URLs (Compose published ports, or whatever your orchestrator mapped).

TypeScript (workspace package `sdk/typescript`):

```ts
import { BuwizComputer } from "buwiz-computer";

const box = BuwizComputer.connect(
  "http://127.0.0.1:1337",
  "http://127.0.0.1:1340",
  process.env.BUWIZ_COMPUTER_TOKEN!,
);
await box.exec({ command: ["echo", "ok"] });
```

Python (`sdk/python`):

```python
import os
from buwiz_computer import BuwizComputer

box = BuwizComputer.connect(
    "http://127.0.0.1:1337",
    "http://127.0.0.1:1340",
    os.environ["BUWIZ_COMPUTER_TOKEN"],
)
box.exec(command=["echo", "ok"])
```

Rust (`crates/buwiz-computer`):

```rust
let box_client = buwiz_computer::BuwizComputer::connect(
    "http://127.0.0.1:1337",
    "http://127.0.0.1:1340",
    std::env::var("BUWIZ_COMPUTER_TOKEN").unwrap(),
);
```

There is no `Sandbox.create()`. Start the guest with Docker (or your own runtime), then connect.

## 2. Native daemons (no desktop)

```bash
./scripts/run-local.sh
```

Binds exec/host to **127.0.0.1**. Does not print `BOX_TOKEN`. CUA will not screenshot. Smoke: `./scripts/smoke-native.sh`.

## 3. Demos (optional)

Not part of the supported product. Ports are uncommon on purpose. Both apps bind **127.0.0.1**.

**EnsureBox** (demo orchestrator) on **43142**:

```bash
cd ensurebox
cp .env.example .env   # ENSUREBOX_TOKEN + ENSUREBOX_ALLOW_INSECURE_DEV=1 for local demo
npm install
npm run dev
```

Needs a built `buwiz-computer:local` image (`docker compose build` from the repo root). It `docker run`s guests, stores `BOX_TOKEN`, and proxies tools. The operator UI is login-gated and shows ports/volumes/lifecycle only — no Screenshot or Shell tab. Docker is invoked as the current user (no sudo fallback).

**L1** (demo human workspace) on **43141**:

```bash
cd l1
cp .env.example .env   # L1_TOKEN + ENSUREBOX_TOKEN (server-side) + insecure-dev flags for local demo
npm install
npm run dev
```

L1 uses `L1_TOKEN` for the human session and `ENSUREBOX_TOKEN` only on the server. It must not call guest binds or read `BOX_TOKEN`. Desktop is screenshots via EnsureBox CUA, not a raw 6080 link.

## 4. Your own orchestrator

1. `docker run` (or equivalent) from this image.
2. Inject `BOX_TOKEN`, `BOX_VNC_PASSWORD`, `BOX_ID`, desktop/chrome/CUA env as needed.
3. Publish 1337, 1340, 6080 on **127.0.0.1** (or a tunnel). Never 5900/9222/8791. Optional `8790` when `BOX_EGRESS_TUNNEL=1` ([EGRESS.md](EGRESS.md)).
4. Poll `GET {hostUrl}/v1/ready` with Bearer until 200.
5. `BuwizComputer.connect(execUrl, hostUrl, token)` — the URLs **you** published, not `/v1/info`.

## Smoke

| Script | What it proves |
| --- | --- |
| `./scripts/smoke.sh` | Image build, ready (Bearer), exec, files, CUA, auth |
| `./scripts/smoke-native.sh` | Host binaries without Docker (loopback binds) |
| `ensurebox/scripts/smoke.sh` | Demo API + login-gated operator UI |
| `l1/scripts/smoke.sh` | L1 has no guest bind / `BOX_TOKEN` / VNC password UI |
