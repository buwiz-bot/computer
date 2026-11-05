# Egress tunnel (`box-egress-v1`)

Guest Chromium can send HTTPS through a client on **your** machine. That is how
a production Bot avoids “the box IP is not a home IP → Facebook email verify”.

This repo owns the **guest** half: `box-egress-tunnel` (MIT, our names). It is
not Cursor `sand-egress-tunnel` and does not re-host any proprietary binary.

The Bot’s own chrome is **out of scope**. The Bot should
read the host setting plus this capability; flipping
`isEgressTunnelAvailable` lives in that repo.

## Why local docker often “just works”

On a laptop, the guest and the browser often share the host’s public IP.
Facebook sees a normal client.

On a **prod VM**, the Bot’s origin and the box’s egress IP are different, and
traffic leaves from a cloud / China (or other) range. Sites force extra checks.

Do **not** assume Docker `network_mode: host` in prod. Publish loopback and
tunnel, same as 1337/1340/6080.

## What is and is not tunneled

| Path | Tunneled when enabled + client attached? |
| --- | --- |
| Chromium (entrypoint + dock `box-chromium`) | **Yes** — `--proxy-server=http://127.0.0.1:8791` |
| `POST /v1/exec` (`curl`, apt, …) | **No** — still the guest’s own egress |
| CDP `127.0.0.1:9222` | **No** — bypass list |

v1 wires Chromium only. That is the Facebook / login-in-the-browser problem.

## Capability JSON (box-host)

`GET /v1/info` (Bearer `BOX_TOKEN` / `BOX_HOST_TOKEN`):

```json
"capabilities": {
  "egress_tunnel": { "enabled": true, "ready": false }
}
```

| Field | Meaning |
| --- | --- |
| `enabled` | `BOX_EGRESS_TUNNEL` is on. The guest is supposed to be running the tunnel server. |
| `ready` | A laptop client is **currently attached**. CONNECT will relay. |

Enabled but no client: `enabled: true, ready: false`. Chromium CONNECT
**fail-closes** (`503`). Honest, not “it might leak out the guest IP”.

`GET /v1/egress` (same Bearer) is the full inventory:

```json
{
  "enabled": true,
  "ready": false,
  "client_attached": false,
  "protocol": "box-egress-v1",
  "ws": "127.0.0.1:8790",
  "proxy": "127.0.0.1:8791"
}
```

`ws` / `proxy` are **container-local listen addresses** (unspecified binds
rewritten to loopback), same honesty as `/v1/info.endpoints`. They are not the
URL the Bot should dial. The Bot already knows how it published **8790**.

`ready` is not on `GET /v1/ready`. Chrome is not either. The box can be ready
for exec while the tunnel has no client.

The Bot:

1. Host setting says the user opted into laptop egress.
2. Box `capabilities.egress_tunnel.enabled` is true (guest actually started the server).
3. Show attach / “Review an action” until `ready` is true.
4. Do not treat guest egress as the user’s home IP until `ready` is true.

## Enable on the guest (prod, no host-network)

1. Long random bearer, **not** `BOX_TOKEN` (the WS port is a different
   exposure). Prefer a file:

   ```bash
   openssl rand -base64 32 > /tmp/egress-bearer
   # compose / k8s: mount it and set BOX_EGRESS_TUNNEL_BEARER_FILE
   ```

2. Env:

   | Variable | Default | Meaning |
   | --- | --- | --- |
   | `BOX_EGRESS_TUNNEL` | `0` | `1` starts the server **before** Chromium and adds proxy flags |
   | `BOX_EGRESS_WS_BIND` | image `0.0.0.0:8790` | WS for the laptop client |
   | `BOX_EGRESS_PROXY_BIND` | `127.0.0.1:8791` | HTTP CONNECT for Chromium. **Never publish.** |
   | `BOX_EGRESS_ADMIN_BIND` | `127.0.0.1:8792` | Unauthenticated status for box-host. Loopback only. |
   | `BOX_EGRESS_PROXY_PORT` | port of `PROXY_BIND` | Used in `--proxy-server` |
   | `BOX_EGRESS_TUNNEL_BEARER` | required when enabled | WS Bearer. Prefer `_FILE`. |
   | `BOX_EGRESS_TUNNEL_BEARER_FILE` | unset | Preferred. Wins over the value. Guest daemon unlinks a **staged** copy. |
   | `BOX_EGRESS_RELAY_HOSTS` | empty = allow all | Comma-separated CONNECT hosts; `*.fbcdn.net` ok |

   Short / well-known bearers follow `BOX_TOKEN`: rejected unless
   `BOX_ALLOW_INSECURE_DEV=1` **and** the WS bind is loopback.

3. Compose already publishes `127.0.0.1:8790:8790`. **Never** publish 8791 or
   8792. Reach WS from a laptop with:

   ```bash
   ssh -N \
     -L 1337:127.0.0.1:1337 \
     -L 1340:127.0.0.1:1340 \
     -L 6080:127.0.0.1:6080 \
     -L 8790:127.0.0.1:8790 \
     user@box-vm
   ```

   TLS in front of 8790 (`wss://`) is a reverse proxy job, same as exec/host.

4. Example (after `write-secrets.sh` wrote `secrets/box_egress_tunnel_bearer`
   because `BOX_EGRESS_TUNNEL_BEARER` was in `.env`):

   ```yaml
   environment:
     BOX_EGRESS_TUNNEL: "1"
     BOX_EGRESS_TUNNEL_BEARER_FILE: /run/secrets/box_egress_tunnel_bearer
   secrets:
     - box_token
     - box_vnc_password
     - box_egress_tunnel_bearer
   # top-level:
   # secrets:
   #   box_egress_tunnel_bearer:
   #     file: ./secrets/box_egress_tunnel_bearer
   ```

   The default `docker-compose.yml` does **not** require that secret file
   (tunnel is off). Add the mount when you turn it on.

## Client on the operator machine (the Bot spawns this)

Binary: `box-egress-tunnel` (workspace crate; copy it next to the Bot or
`cargo run -p box-egress-tunnel`).

```bash
box-egress-tunnel client \
  --url ws://127.0.0.1:8790 \
  --bearer-file /path/to/egress-bearer
```

Flags / env:

| Flag | Env | Notes |
| --- | --- | --- |
| `--url` | `BOX_EGRESS_CLIENT_URL` | `ws://` or `wss://`. Required. |
| `--bearer` | `BOX_EGRESS_TUNNEL_BEARER` | Avoid; prefer file. |
| `--bearer-file` | `BOX_EGRESS_TUNNEL_BEARER_FILE` | Wins over `--bearer`. **Not** unlinked (you still need it). |
| `--relay-hosts` | `BOX_EGRESS_RELAY_HOSTS` | Optional second allowlist (also enforced on the server). |
| `--allow-host` | (repeatable) | Extra host patterns. |
| `--reconnect` | `BOX_EGRESS_RECONNECT=1` | Retry on drop; **not** on `401`. |

Auth: `Authorization: Bearer …` on the WS handshake. `X-Box-Egress-Bearer` is
accepted if a proxy eats `Authorization`. No query-string token (logs).

While attached, `/v1/info` → `capabilities.egress_tunnel.ready: true`.

If the Bot’s WS URL is `wss://host.example/egress`, the proxy must forward
the Bearer header and WebSocket upgrades. Path may be `/` or anything; the
server does not care.

## Wire (`box-egress-v1`)

Not a byte-for-byte copy of any undocumented proprietary protocol.

Handshake: HTTP WebSocket on the WS bind. Optional
`Sec-WebSocket-Protocol: box-egress-v1`.

**Control** = WS text, JSON, tagged `type`, always `"v": 1`:

```json
{"v":1,"type":"hello","role":"server","protocol":"box-egress-v1"}
{"v":1,"type":"open","id":1,"host":"example.com","port":443}
{"v":1,"type":"opened","id":1}
{"v":1,"type":"error","id":1,"message":"connection refused"}
{"v":1,"type":"close","id":1,"reason":"eof"}
```

Server → client `open` for each Chromium CONNECT (and for absolute-form HTTP
proxy requests). Client dials TCP, replies `opened` or `error`.

**Data** = WS binary: 4-byte big-endian stream `id`, then payload. TLS stays
end-to-end between Chromium and the site; the laptop is a TCP hop.

One client at a time. A new successful handshake replaces the previous.

CONNECT responses: `503` no client / too many streams, `403` allowlist,
`502` client dial error, `504` client did not `opened` in 15s, `200 Connection
Established` when the stream is up.

## Manual smoke (internet OK here; CI stays local)

Automated tests are local: server + client + TCP echo, no public net.

On a real box, after the client is attached:

1. `GET /v1/egress` → `ready: true`.
2. In guest Chromium (or CDP), open an echo-IP URL, e.g.
   `https://api.ipify.org?format=json` (or any “what is my IP” that you trust).
3. The reported IP should be the **laptop’s** egress, not the VM’s.
4. Detach the client. The same page should fail or hang (fail-closed), and
   `ready` goes false. It must **not** silently fall back to the VM IP.

## Layout

```
crates/box-egress-tunnel   server + client + probe types for box-host
```

Guest image: `box-egress-tunnel` next to `box-exec` / `box-host`. Chromium is
still started only by `docker/entrypoint.sh`.
