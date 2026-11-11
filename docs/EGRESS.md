# The browser's network: the proxy, its rules, and the tunnel (`box-egress-v1`)

A box can put its browsers behind a CONNECT proxy of its own, `box-egress-tunnel`, which judges
every page load by the **computer's rules** and then reaches the site one of two ways:

- **direct**: the box dials the site itself, from its own network;
- **tunnel**: through a client on **your** machine, so the site sees your machine's address. That
  is how a production Bot avoids "the box IP is not a home IP → Facebook email verify".

The rules mean the same thing whichever the exit, and they can change while the box runs.

This repo owns the **guest** half (`box-egress-tunnel`, MIT, our names) and the client
(`box-egress`, a small library the Bot's broker runs, and `box-egress-tunnel client`). It is not
Cursor `sand-egress-tunnel` and does not re-host any proprietary binary.

## What is and is not covered

| Path | Through the proxy and the rules? |
| --- | --- |
| Chromium on every screen of the box: the main screen and every Bot's own (`entrypoint.sh`, `box-chromium`) | **Yes**, `--proxy-server=http://127.0.0.1:8791`, when the proxy is on |
| `POST /v1/exec` (`curl`, apt, ...), and any program not started as the screen's browser | **No**: the guest's own network. The rules do not apply to them. |
| Loopback (`localhost`, `127.0.0.1`, `[::1]`) from Chromium | **No**: Chromium's bypass list sends it straight to the box's own services |

The rules are **per box**: a box has one proxy and one tunnel, and every screen's browser on it,
whichever Bot's, goes through them. WebRTC is held to the proxy too
(`--force-webrtc-ip-handling-policy=disable_non_proxied_udp`).

Plain `http://` pages are **refused** through the proxy, whichever the exit (`405`, a page that
says so): the proxy relays CONNECT only. `https://` pages load.

## The rules

A rules document, as `PUT /v1/egress/rules` takes it and `GET /v1/egress/rules` gives it:

```json
{"v": 1, "version": 7, "exit": "direct", "mode": "all", "allow": [], "block": ["facebook.com"]}
```

| Field | Meaning |
| --- | --- |
| `v` | Always `1`. Anything else is refused. |
| `version` | The sender's number for this set, echoed back so it can tell its own set took. `0` or absent: box-host picks one. |
| `exit` | `direct` or `tunnel`. `tunnel` on a box made without it is refused (`409 egress_tunnel_off`). |
| `mode` | `all`: every site except the blocked ones. `listed`: only the allowed sites that aren't blocked. |
| `allow`, `block` | Site names, at most 1000 each. |

Matching, exactly:

- An entry names a site **and every name under it**: `facebook.com` covers `facebook.com` and
  `www.facebook.com`, never `notfacebook.com`. A leading `*.` means the same (`*.example.com` is
  stored as `example.com`). There is no other wildcard.
- Entries and hosts compare in lower case, with one trailing dot dropped. An international name is
  written in its `xn--` form, which is what a browser sends; any other non-ASCII entry is refused,
  as is a host that isn't a valid name or address (`400 bad-host`).
- An entry of `*`, an empty entry, an address (`10.0.0.1`, `::1`), and one with a scheme, port or
  path are **refused** when the rules are set (`422 invalid_rules`, naming the list and position).
  They never mean "everything".
- **Block always wins.** A bare address (`93.184.216.34`, `[2606::1]`) matches no name, and a
  rule can't tell which site an address is. So **with any site blocked, every bare address is
  refused** (`403 address`), at the box's proxy and at the client alike: a blocked site isn't
  reachable by its number. With nothing blocked, under `all` an address is reached unless the
  address policy refuses it; under `listed` it is refused.
- **No rules** (never set, or a file that doesn't read) refuses every site (`403 no-rules`). It
  never means "allow".

After the rules, the address: whichever end dials resolves the name with a 4 s timeout, judges
**every address it resolved to**, and dials only one that passed, never the name a second time (no
DNS-rebinding hole). Refused, whatever the rules say: loopback, private (RFC 1918), link-local
(including `169.254.169.254`), carrier-grade NAT (`100.64.0.0/10`), `0.0.0.0/8`, `192.0.0.0/24`,
benchmarking (`198.18.0.0/15`), documentation, multicast, `240.0.0.0/4` and broadcast; IPv6
loopback, unspecified, unique-local, link-local, site-local (`fec0::/10`), multicast, discard
(`100::/64`), documentation, and every form that carries an IPv4 address (mapped, compatible,
NAT64 `64:ff9b::/96` and `64:ff9b:1::/48`, 6to4 `2002::/16`, Teredo `2001::/32`). Ports: `80` and
`443` unless `BOX_EGRESS_RELAY_PORTS` says otherwise.

### What the proxy answers

Every refusal carries `X-Box-Egress-Reason`, one stable word:

| Status | Reason | When |
| --- | --- | --- |
| `200 Connection Established` | | The stream is up |
| `400` | `bad-request`, `bad-host` | Not a request the proxy reads; not a valid host |
| `408` | `slow-request` | The request head didn't come within 5 s |
| `403` | `no-rules` | No rules are set |
| `403` | `blocked` | On the block list |
| `403` | `not-listed` | `listed` mode, not on the allow list |
| `403` | `address` | A bare address, and some site is blocked |
| `403` | `relay-hosts` | Not in the operator's static `BOX_EGRESS_RELAY_HOSTS` |
| `403` | `destination` | The address it resolved to is refused, or the port isn't relayed (direct exit) |
| `405` | `plain-http` | Absolute-form `http://`: CONNECT only |
| `502` | `resolve`, `connect`, `client-error` | The name didn't resolve; the site didn't answer; the client couldn't open it (its words, which name no site) |
| `503` | `no-client` | The exit is the tunnel and no client is attached. Never dialled from the box instead. |
| `503` | `streams-full` | 256 streams in use |
| `503` | `busy` | Too many connections open (576), or waiting to be read (64) |
| `504` | `timeout`, `open-timeout` | The site, or the client, didn't answer in time |

### Changing the rules while the box runs

`PUT /v1/egress/rules` (box-host, Bearer) checks the set, writes it to
`/run/box/egress-rules/rules.json` (a folder only box-host's user can write; box-prepare made it),
and asks the proxy to read it again on its loopback admin port. It answers `200` with the rules'
counts only once the proxy reports that same version in force. A set that doesn't check out is
refused whole (`422`) and changes nothing.

The box **starts** with the rules in `BOX_EGRESS_RULES_FILE`, which whoever starts the box stages
(Buwiz Bot's broker stages it beside the box's token, from its own copy, on every start). box-keeper
copies it into place before anything else runs, so there is no moment with a browser and no rules.
A staged file that doesn't read leaves no rules: every site is refused until a `PUT` sets some. A
box with the proxy on (`BOX_EGRESS_PROXY=1`, or `BOX_EGRESS_TUNNEL=1`) and **no** rules file
starts the same way, whichever the exit: no rules is no site. To browse every site through the
tunnel as such a box did before, stage `{"v":1,"exit":"tunnel","mode":"all"}`.

**A restart returns to the staged file.** Rules set with `PUT` live in `/run/box`, which the box
makes afresh at every start, so a box restarted by anything else comes back on whatever
`BOX_EGRESS_RULES_FILE` holds then (or with no rules, if nothing is staged). Buwiz Bot's broker
stages its latest set before every start it makes, and puts it in force again after every wake.
`GET /v1/egress` says where the rules in force came from: `rules_origin` is `staged` (the file the
box started with) or `set` (a `PUT` since), beside the set's `version`.

`PUT` takes one set at a time. It answers `200` only once this version is in force; if the proxy
has another version in force it's `409 rules_superseded` (with that version), if the proxy refuses
the file `422 rules_refused`, and with no proxy answering `503 egress_not_running`. In each of those
the file is put back as it was: it's never ahead of what's in force. When the proxy's answer to the
reload doesn't come, box-host asks it what's in force before answering: the new set is a `200`,
another is answered as above, and a proxy that can't be asked at all is `503 rules_state_unknown`,
with the file left at the last set the proxy was known to hold.

## Turning it on

| Variable | Default | Meaning |
| --- | --- | --- |
| `BOX_EGRESS_PROXY` | `0` | `1` starts the proxy before Chromium and puts every screen's browser behind it, under the rules. No WebSocket, no bearer. |
| `BOX_EGRESS_TUNNEL` | `0` | `1` adds the tunnel's WebSocket (and turns the proxy on). Needs a bearer. |
| `BOX_EGRESS_RULES_FILE` | unset | The rules the box starts with. Read by box-keeper. |
| `BOX_EGRESS_WS_BIND` | image `0.0.0.0:8790` | WS for the laptop client |
| `BOX_EGRESS_PROXY_BIND` | `127.0.0.1:8791` | The CONNECT proxy. **Loopback only**: anything else is refused at start, since it dials out for whoever reaches it. **Never publish.** |
| `BOX_EGRESS_ADMIN_BIND` | `127.0.0.1:8792` | Status and rule reloads for box-host. **Loopback only**, refused otherwise. |
| `BOX_EGRESS_PROXY_PORT` | port of `PROXY_BIND` | Used in `--proxy-server` |
| `BOX_EGRESS_TUNNEL_BEARER_FILE` | unset | The WS bearer, as a file. Preferred. |
| `BOX_EGRESS_TUNNEL_BEARER` | | The bearer as a value. Avoid: it stays in `/proc/1/environ`. |
| `BOX_EGRESS_RELAY_HOSTS` | empty | The operator's static name filter, on top of the rules, at both ends. A name covers itself and every name under it, as in the rules (`*.x` means `x`). Unset or empty is no extra filter; `*` alone is none on purpose; a value that names nothing usable (`*.`, a lone comma) stops the process at start. |
| `BOX_EGRESS_RELAY_PORTS` | `80,443` | Ports either end will dial; `*` is any. An entry that isn't a port stops the process at start. |
| `BOX_EGRESS_ALLOW_PRIVATE` | unset | `1` lets either end dial loopback, private and other non-public addresses. Leave it off; the image smoke uses it to reach its fixture on loopback. |
| `BOX_EGRESS_BEARER_STAGED` | set by `box-keeper` | The staged copy of the bearer the entrypoint hands to the server, which unlinks it. Do not set it. |
| `BOX_EGRESS_RULES_PATH` | `/run/box/egress-rules/rules.json` | Where the rules live in the box. Leave it. |

The bearer: a long random value, **not** `BOX_TOKEN` (the WS port is a different exposure).
Short or well-known bearers follow `BOX_TOKEN`: refused unless `BOX_ALLOW_INSECURE_DEV=1` and the
WS bind is loopback.

```bash
umask 077
mkdir -p ~/.config/buwiz && openssl rand -base64 32 > ~/.config/buwiz/egress-bearer
```

Compose already publishes `127.0.0.1:8790:8790`. **Never** publish 8791 or 8792. Reach the WS
from a laptop with `ssh -N -L 8790:127.0.0.1:8790 user@box-vm` (beside 1337, 1340, 6901). TLS in
front of 8790 (`wss://`) is a reverse proxy's job, as for exec and host.

With Compose, mount the bearer as a secret and set `BOX_EGRESS_TUNNEL_BEARER_FILE` to it. The
default `docker-compose.yml` doesn't require the secret file (the tunnel is off).

The proxy's ports are above the box's port floor, so box-keeper starts `box-exec`, and with it any
command, only once the proxy listens: a command can never take one of its ports first.

### What the tunnel exposes

The WS (8790) is published on the host's **loopback** only. Anything on that host that can reach
loopback can try it; the bearer guards it. A wrong or missing bearer is held 1 s before its `401`
and logged (without what was presented). It is held apart from the handshakes still under way, so
however many wrong bearers wait out their second, a right one gets in at once; at most 64 are held
at a time, and past that one is closed with no answer, which bounds guessing at 64 a second
against a bearer of 32 random bytes. One client at a time: a newer
successful attach replaces the older and is logged. The proxy and admin ports are never published.

## Capability JSON (box-host)

`GET /v1/info` (Bearer):

```json
"capabilities": {
  "egress_tunnel": { "enabled": true, "ready": false },
  "egress_proxy": { "enabled": true, "ready": true }
}
```

| Field | Meaning |
| --- | --- |
| `egress_tunnel.enabled` | The box was made with the tunnel. |
| `egress_tunnel.ready` | A client is **attached** right now. |
| `egress_proxy.enabled` | The browser goes through the proxy (`BOX_EGRESS_PROXY`, or the tunnel). |
| `egress_proxy.ready` | The proxy answers, with rules set. |

A box with the proxy answers `GET /v1/ready` only once the proxy does.

`GET /v1/egress` (Bearer) is the full status. It names **no site**: the rules are counts, and
`counts` are outcomes since the proxy started, every refusal counted.

```json
{
  "enabled": true, "ready": false, "client_attached": false,
  "protocol": "box-egress-v1", "ws": "127.0.0.1:8790", "proxy": "127.0.0.1:8791",
  "proxy_enabled": true, "proxy_up": true,
  "rules": { "version": 7, "exit": "tunnel", "mode": "all", "allow": 0, "block": 1 },
  "rules_error": null,
  "rules_origin": "set",
  "can_browse": false,
  "counts": { "direct": 0, "tunneled": 12, "blocked": 3, "not_listed": 0, "no_rules": 0,
              "no_client": 2, "streams_full": 0, "destination": 0, "failed": 1, "plain_http": 0 }
}
```

`can_browse`: rules are set, and their exit is `direct` or has a client attached. With the tunnel as
the exit and no client, the browser reaches no site, and says nothing louder than the proxy's `503`:
whoever runs the box should say so plainly.

## The client on the operator's machine

Buwiz Bot's broker runs the `box-egress` library's client in its own process, one per computer.
For anyone else, the same client is a command:

```bash
box-egress-tunnel client \
  --url ws://127.0.0.1:8790 \
  --bearer-file /path/to/egress-bearer \
  --reconnect
```

| Flag | Env | Notes |
| --- | --- | --- |
| `--url` | `BOX_EGRESS_CLIENT_URL` | `ws://` or `wss://`. Required. |
| `--bearer-file` | `BOX_EGRESS_TUNNEL_BEARER_FILE` | Read and **kept** (you still need it). |
| | `BOX_EGRESS_TUNNEL_BEARER` | The value. Avoid. No flag takes the value: argv is in `ps`. |
| `--relay-hosts` | `BOX_EGRESS_RELAY_HOSTS` | The static name filter. |
| `--allow-host` | (repeatable) | Extra static entries. |
| `--reconnect` | `BOX_EGRESS_RECONNECT=1` | Attach again after a drop, waiting 1 s doubling to 30 s, reset by each attach. Never after a `401`, told by its status. |

The client judges every stream again before it dials: by the computer's rules (the broker's
client holds the computer's own; the command's has every site), by the static filter, then by the
resolved address, as above. On the operator's machine it is the authority for traffic leaving
that network. A stream whose destination stops reading is closed on its own; it never holds up the
others, and the reader never waits on the box. It dials at most 256 streams at once for a
session, the guest's own cap; one past that is refused (`too many streams`). When a session ends, every stream of it ends.

Auth: `Authorization: Bearer …` on the WS handshake. `X-Box-Egress-Bearer` is accepted if a proxy
eats `Authorization`. No query-string token (logs).

## What is logged

Neither end logs a site, at any level: browsing hosts are private. The proxy logs each refusal's
status and reason word, and attaches, detaches, evictions and refused handshakes. The client logs
attaches, detaches and each refusal's reason word. `counts` in `GET /v1/egress` are the outcomes.

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

Server → client `open` for each CONNECT the rules allow with the tunnel as the exit. The client
dials TCP, replies `opened` or `error`.

**Data** = WS binary: 4-byte big-endian stream `id`, then payload. TLS stays end-to-end between
Chromium and the site; the laptop is a TCP hop.

Keepalive: each end pings an idle peer every 15 s and drops one silent for 30 s. Frames at most
256 KiB, messages 1 MiB, data chunks 32 KiB.

## Manual check (internet OK here; CI stays local)

Automated tests are local: server, client and a loopback echo, no public net. The image smoke
(`scripts/smoke.sh`) proves the rules with direct exit, a change at runtime, a Bot screen's browser
going through the proxy, the tunnel with no client (`503`), and with a client attached, all against
the fixture shop on the box's loopback.

On a real box with the tunnel as the exit, after the client is attached:

1. `GET /v1/egress` → `ready: true`, `can_browse: true`.
2. In guest Chromium, open an echo-IP page you trust.
3. The reported IP should be the **laptop's** egress, not the VM's.
4. Detach the client. The same page should fail (fail-closed), and `ready` goes false. It must
   **not** fall back to the VM's address.

## Layout

```
crates/box-egress          the wire, the rules, the address policy, the client
crates/box-egress-tunnel   the guest server (proxy, WS, admin) and the CLI
```
