# box PR #30 review — `box-egress-tunnel` (tip f62fb6a, diff vs origin/main)

Reviewed: the whole of `crates/box-egress-tunnel/` (server, client, http1, protocol, allowlist, config, status, main, relay_tests), `docker/entrypoint.sh`, `docker/box-chromium`, `docker/Dockerfile`, `docker-compose.yml`, `crates/box-common/src/{config,auth,listen}.rs`, `crates/box-host/src/lib.rs`, `scripts/{write-secrets,smoke,smoke-native,check-openapi}.sh`, `docs/EGRESS.md` + the other touched docs, `docs/openapi.yaml`, both SDKs.

**Claims that check out** (no finding): the bearer compare is constant-time (`box_common::tokens_equal` → `subtle::ConstantTimeEq`, with a length-mismatch dummy compare); an empty/unset bearer cannot mean "no auth" (`load_bearer`/`from_env` both hard-error on empty, and `authorized()` returns `false` when no credential is presented); compose really does publish only `127.0.0.1:8790:8790` and nothing anywhere publishes 8791/8792; "tunnel server before Chromium" is a real readiness gate (`start_egress` polls `/v1/status` up to 5 s and `exit 1`s), not a `sleep`; the Rust tests do cover the fail-closed 503, the 403 allowlist reject and the wrong-bearer 401 — not only the happy round-trip; no token appears in argv or logs on the entrypoint path (`--bearer-file` with a staged 0400 copy).

---

## [SEVERITY: critical] [CONFIDENCE: high] CONNECT proxy has no destination filtering — any page in the guest browser can reach the operator's laptop localhost, home LAN and link-local

**Where:** `crates/box-egress-tunnel/src/server.rs:337` (only gate), `crates/box-egress-tunnel/src/allowlist.rs:52-64`, `crates/box-egress-tunnel/src/client.rs:180-187`, `docker-compose.yml:48` (`BOX_EGRESS_RELAY_HOSTS: ${...:-}`)

**What's wrong:** The only destination check is `shared.cfg.allowlist.allows(&req.host)`, and `Allowlist::parse("")` returns an empty pattern list whose `allows()` returns `true` for everything (`allowlist.rs:53-55`). The shipped default — image env, compose env and `.env.example` — is empty, i.e. **allow all**. There is no IP/CIDR filtering at all: the laptop client does `TcpStream::connect((host.as_str(), port))` for whatever the guest asks for, including `127.0.0.1`, `localhost`, `[::1]`, `10/8`, `172.16/12`, `192.168/16`, `169.254.169.254` and `[fd00::]`. There is no port restriction either, so 22/3389/5432/2375 are all reachable. Note also `--proxy-bypass-list=...;<-loopback>` (`docker/entrypoint.sh:238`) *removes* Chromium's implicit link-local bypass, so `http://169.254.169.254/` is sent to the proxy rather than resolved in the guest.

**Why it matters / how it fails:** The attacker is not only the agent — it is any web content the agent's browser renders, because every browser request goes through 127.0.0.1:8791. A malicious page (or an ad, or reflected XSS on a site the agent visits) does `fetch('http://192.168.1.1/setup.cgi?...', {mode:'no-cors'})` → guest Chromium → CONNECT/absolute-form to the proxy → WS → **operator's laptop dials the operator's own router**. Same for `http://127.0.0.1:2375/containers/json` (laptop Docker API → root on the laptop), `http://127.0.0.1:11434`, a local Grafana/Jupyter, or `http://169.254.169.254/latest/meta-data/iam/security-credentials/` if the operator machine is a cloud VM. Blind SSRF (side effects, timing) always works; anything returning permissive CORS is fully readable; with CONNECT the browser can speak arbitrary TCP to LAN ports. This is a home-network pivot triggered by untrusted web content, and it is on by default whenever the tunnel is on.

**Suggested fix:** Filter on the **resolved address**, not the host string, on the client side (that is where the dial happens): resolve first, reject loopback/private/link-local/CGNAT/multicast/unspecified unless an explicit `--allow-private` opt-in, and connect to the vetted `SocketAddr` (not by name again — otherwise DNS rebinding reintroduces it). Default the port set to `{80,443}` with an opt-in override. Make `BOX_EGRESS_RELAY_HOSTS` empty mean "public unicast only", and document the residual exposure in `docs/EGRESS.md`.

---

## [SEVERITY: high] [CONFIDENCE: high] One `accept()` error on 8790 takes down the whole container — and there is no handshake timeout or connection cap to prevent it

**Where:** `crates/box-egress-tunnel/src/server.rs:108-136` (`accepted?` in all three arms), `server.rs:166-190` (`accept_hdr_async` with no timeout), `docker/entrypoint.sh:298` + `361-370` (death-watch)

**What's wrong:** In `run_with_binds` every accept arm does `let (stream, peer) = accepted?;` — any `io::Error` from `accept()` ends the whole server loop and the process exits. `accept()` errors are routinely transient (`ECONNABORTED`) or resource-driven (`EMFILE`/`ENFILE`), and nothing here limits inbound connections: `handle_ws` awaits `accept_hdr_async` with no timeout, so a peer that opens a TCP connection and never sends the upgrade request pins one fd + one task forever. `record $!` in the entrypoint puts the tunnel in the death-watch, so when the tunnel exits, `shutdown` runs and the **container exits**.

**Why it matters / how it fails:** Anyone who can reach 8790 — host loopback (the published port), any container on the compose network, anyone the operator SSH-forwarded `-L 8790` to — opens a few thousand TCP connections and sends nothing. No bearer needed; auth happens after accept. The process hits its fd limit, the next `accept()` returns `EMFILE`, `?` propagates, `box-egress-tunnel` exits, the entrypoint watchdog sees the dead pid within 1 s and tears the box down. Unauthenticated remote kill of an entire box, plus it collaterally kills box-exec/box-host. Even without an attacker, a single spurious `ECONNABORTED` does it.

**Suggested fix:** Log-and-continue on accept errors (with a short backoff on `EMFILE`), never `?`. Wrap `accept_hdr_async` in a `timeout` (5-10 s), cap concurrent pre-handshake connections and concurrent sessions with a semaphore, and consider not death-watching the tunnel (or restarting it) so a tunnel fault does not destroy exec/files availability.

---

## [SEVERITY: high] [CONFIDENCE: medium] Absolute-form HTTP proxying pins a reused Chromium proxy socket to the first host — later requests for other origins are delivered to the wrong server

**Where:** `crates/box-egress-tunnel/src/http1.rs:120-140` (`rewrite_origin_form`), `crates/box-egress-tunnel/src/server.rs:416-430` (relay after the head)

**What's wrong:** For non-CONNECT requests the proxy parses only the **first** request line, opens one upstream TCP connection to that host, forwards the rewritten head, and then relays raw bytes for the life of the client socket. It strips `Proxy-Connection` but never adds `Connection: close` in either direction and never closes the client socket after the first response. Chromium's socket pool keys non-tunnelled `http://` connections on the *proxy*, not the origin, and reuses a keep-alive proxy socket across origins.

**Why it matters / how it fails:** Guest Chromium fetches `http://a.example/x` (proxy opens TCP→a.example, relays). `a.example` answers with `Connection: keep-alive`, so Chromium keeps the proxy socket. Chromium then issues `GET http://b.example/private HTTP/1.1` on the same socket — those bytes are relayed verbatim into the still-open connection to **a.example**, along with `b.example`'s cookies, `Authorization`, and any POST body. That is a credential/request leak to an unrelated origin plus a silent correctness failure (wrong page rendered as `b.example`). It also makes any `BOX_EGRESS_RELAY_HOSTS` allowlist meaningless past the first request on a connection.

**Suggested fix:** For the non-CONNECT path, force `Connection: close` in the forwarded head, strip hop-by-hop headers (`Connection`, `Keep-Alive`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`, `Proxy-Authorization`), and close the client socket after the first exchange; or drop absolute-form support entirely and answer 405 (Chromium will CONNECT for https, which is the stated use case).

---

## [SEVERITY: high] [CONFIDENCE: high] No WebSocket keepalive: `ready: true` survives a dead laptop for hours, and CONNECT hangs 15 s instead of fail-closing

**Where:** `crates/box-egress-tunnel/src/server.rs:217-291` (session loop — no ping, no read deadline), `crates/box-egress-tunnel/src/client.rs:107-166`, `server.rs:24` (`OPEN_TIMEOUT: 15s`), `docs/EGRESS.md:49-50`

**What's wrong:** Neither side ever sends a Ping and neither enforces an idle/read deadline. `attached` flips to false only when the WS stream yields `None`/`Close`/`Err`, i.e. only on a clean FIN or an RST. A suspended laptop, a dropped Wi-Fi link, a NAT/VPN timeout or a killed `-9` client behind a stateful firewall produce none of those; the guest sits in `stream.next()` until TCP keepalive (default ~2 h on Linux, and the socket has no `SO_KEEPALIVE` set) or a write eventually fails.

**Why it matters / how it fails:** Operator closes the laptop lid mid-session. `/v1/info` and `/v1/egress` keep reporting `ready: true, client_attached: true` — the exact opposite of the "honest, not 'it might leak'" promise in `docs/EGRESS.md:49-50`. Every new CONNECT is accepted, the `open` control frame disappears into the TCP send buffer, and Chromium hangs for the full `OPEN_TIMEOUT` (15 s) before getting a `504` — so each page load stalls 15 s per connection instead of failing fast with 503, and an orchestrator polling `ready` never learns to re-attach.

**Suggested fix:** Server-side ping every ~15 s with a missed-pong deadline (~2 missed pings ⇒ drop the session and clear `attached`); same on the client so it can trigger its reconnect loop. Set TCP keepalive on the accepted socket as a backstop. Consider making `ready` additionally require a recent successful pong.

---

## [SEVERITY: high] [CONFIDENCE: high] Any second authenticated WS client can inject bytes into, and tear down, the first client's live streams

**Where:** `crates/box-egress-tunnel/src/server.rs:267-279` (binary frames), `server.rs:293-322` (`on_control`), `server.rs:224-233` (session overwrite), `docs/EGRESS.md:193`

**What's wrong:** `serve_client` is per-connection, but both the data path and `on_control` look up `shared.session` — the *globally current* session — and never check that the frame arrived on the connection that owns it (`generation` is only consulted at teardown, `server.rs:287`). Attaching a second client simply overwrites `*g` (`server.rs:226`) with no check for an existing session, no close sent to the incumbent, and no log at `info` that a takeover happened.

**Why it matters / how it fails:** Two consequences. (a) **Hijack:** anyone holding the bearer attaches and instantly becomes the exit node for all subsequent browsing — they see every destination host, and can read/modify any plaintext HTTP the agent does — while the legitimate operator's client stays connected, believes it is attached, and is silently orphaned. All the incumbent's in-flight CONNECTs die at the same moment (their `StreamSlot`s are dropped with the old session). (b) **Cross-session injection:** the evicted (or merely concurrent) connection can still send `{"v":1,"type":"close","id":N}` to kill the current client's streams, `{"type":"opened","id":N}` to make a CONNECT return 200 before the remote TCP exists, or a binary frame `[0,0,0,N] + payload` to write attacker bytes into stream N's socket — i.e. response injection into whatever Chromium is loading over plaintext, and stream ids are sequential from 1 (`alloc_id`, `server.rs:435-446`), so no guessing is needed. `docs/EGRESS.md:193` claims "One client at a time. A new successful handshake replaces the previous" — the replacement is not enforced on the wire.

**Suggested fix:** Tag every frame handler with the session `generation` and ignore frames whose generation is not current; explicitly close the incumbent WS (Close frame + abort) when a new client takes over, and log the takeover at `warn` with both peers. Better still, reject a second concurrent handshake with 409 unless an explicit `--allow-takeover` is set.

---

## [SEVERITY: high] [CONFIDENCE: high] "Fail-closed" is a Chromium command-line flag, not an enforced property — everything else in the guest still exits via the VM

**Where:** `docker/entrypoint.sh:233-240`, `docker/box-chromium:6-17`, `docs/EGRESS.md:24-32`

**What's wrong:** The only thing routing traffic through the tunnel is `--proxy-server` on the one Chromium instance the entrypoint starts. There is no netns/iptables/`REDIRECT` enforcement and no managed-policy `ProxyMode`. Consequently: (1) `docker/box-chromium` re-derives the flags from `$BOX_EGRESS_TUNNEL` in the launcher's environment — if that variable is not inherited (a shell that did not come from the entrypoint's XFCE session, a desktop launcher edited later, a `docker exec`), a cold start gets **no proxy** and browses straight out of the VM; (2) anything can start a second browser with a different `--user-data-dir` and no proxy flag; (3) an agent driving CDP on 127.0.0.1:9222 can call `Target.createBrowserContext` with `proxyServer: "direct://"` and get an unproxied context in the *same* browser; (4) Chromium's WebRTC/UDP path ignores HTTP proxies unless `WebRtcIPHandlingPolicy`/`--force-webrtc-ip-handling-policy=disable_non_proxied_udp` is set, so a page using WebRTC leaks the VM's real IP and can exfiltrate over UDP.

**Why it matters / how it fails:** The PR's central promise is "Chromium must NOT silently fall back to exiting via the VM". A site doing IP-geo or anti-bot checks sees the laptop IP over HTTPS and the VM IP via a WebRTC STUN candidate in the same page, which is a *worse* signal than either alone. And the failure mode for (1)/(2) is silent: nothing reports "this browser is not proxied" — `/v1/egress` still says `ready: true`.

**Suggested fix:** Add `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` (or the managed policy) to both launch paths. Make `docker/box-chromium` refuse to start (or exit non-zero with a clear message) when `BOX_EGRESS_TUNNEL` is on but it cannot determine the proxy port, rather than falling through unproxied. Consider a nftables/iptables owner rule in the guest that DROPs outbound TCP except to the proxy when the tunnel is enabled, which is the only way to make "fail-closed" true rather than aspirational — and state plainly in `docs/EGRESS.md` that CDP can override the proxy per browser context.

---

## [SEVERITY: medium] [CONFIDENCE: high] `?` on the "200 Connection Established" write leaks the stream slot, eventually wedging the tunnel with a misleading 503

**Where:** `crates/box-egress-tunnel/src/server.rs:416-432`

**What's wrong:**
```rust
if req.connect {
    stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;   // <-- early return
}
...
copy_proxy_stream(...).await;
drop_stream(&shared, id);   // <-- skipped on the error path
```
An `io::Error` here returns from `handle_proxy` without ever calling `drop_stream(&shared, id)` and without sending a `close` control frame, so the `StreamSlot` stays in `session.streams` for the life of the session and the laptop-side `copy_outbound` task keeps its TCP connection to the destination open.

**Why it matters / how it fails:** A process in the guest sends `CONNECT x.example:443`, then sets `SO_LINGER 0` and closes so the write gets `ECONNRESET`/`EPIPE`. Repeat 256 times (a one-line loop) and `session.streams.len() >= MAX_STREAMS` is permanently true: every subsequent CONNECT gets `503 "box-egress-tunnel: no client attached"` even though a client *is* attached and `/v1/egress` reports `ready: true` — an operator chasing that message will look at the client, not at slot exhaustion. Each leaked slot also holds an orphaned TCP connection on the operator's machine. The same path can trigger incidentally when Chromium cancels a pending CONNECT with unread data queued.

**Suggested fix:** Replace the `?` with a match that falls through to cleanup, or restructure so `drop_stream` + `close` run from a guard/`scopeguard` (or a `Drop` impl on a small RAII handle) covering every exit from `handle_proxy`. Also make the 503 body distinguish "no client attached" from "stream limit reached".

---

## [SEVERITY: medium] [CONFIDENCE: medium] `drop_stream` and the data path ignore the session generation, so a stale handler can kill a *new* client's live connection

**Where:** `crates/box-egress-tunnel/src/server.rs:448-453`, `server.rs:381`, `server.rs:396`, `server.rs:404`, `server.rs:431`

**What's wrong:** `drop_stream(shared, id)` removes `id` from whatever session is current. Stream ids are per-session and restart from 1 on every new session (`next_id: 0` at `server.rs:230`), so ids collide across sessions.

**Why it matters / how it fails:** Client A is attached with streams 1..5. A drops; its `StreamSlot`s are dropped, so the five `copy_proxy_stream` loops wake up (`recv()` → `None`) and head for their `drop_stream(id)` call. Meanwhile client B attaches (reconnect is 2 s, or a takeover is immediate) and its first CONNECT gets id 1. A's straggler task then calls `drop_stream(1)` and removes **B's** live stream, dropping its `from_client` sender — B's Chromium tab dies mid-request for no visible reason. Timing-dependent but entirely reachable with `--reconnect`.

**Suggested fix:** Carry the `generation` into `handle_proxy` and make `drop_stream(shared, generation, id)` a no-op when the current session's generation differs; do the same for the binary-frame lookup at `server.rs:267-279`.

---

## [SEVERITY: medium] [CONFIDENCE: high] Laptop client leaks tasks and sockets: `?` skips `writer.abort()`, and open streams are never cancelled when the WS drops

**Where:** `crates/box-egress-tunnel/src/client.rs:131-133` (`frame.context("websocket read")?`), `client.rs:146-148` (detached `tokio::spawn`), `client.rs:187` (`TcpStream::connect` with no timeout), `client.rs:62-76` (reconnect loop)

**What's wrong:** Three related leaks. (a) The `?` on the websocket read error returns from `run_once` *before* `writer.abort()` (which only runs on the clean path at `client.rs:168`), so the writer task lives on holding the WS sink and its socket — forever, because `out_tx` clones are still held by spawned `handle_open` tasks so `out_rx.recv()` never returns `None`. (b) `handle_open` tasks are never tracked or aborted; a stream sitting idle in `copy_outbound`'s `select!` has its sender alive in the `streams` map (kept alive by the task's own `Arc<Streams>`), so `from_server.recv()` never returns `None` and the destination TCP connection stays open indefinitely after the tunnel is gone. (c) `TcpStream::connect` has no timeout, so a black-holed destination takes ~2 min of SYN retries while the server gives up at 15 s and sends `close` — the `close` arrives *before* `streams.insert(id, ...)` at `client.rs:191`, so the `remove` is a no-op and the entry is inserted afterwards and never removed.

**Why it matters / how it fails:** With `--reconnect` (the documented prod mode), every network blip leaves behind one writer task, one WS socket and every in-flight destination socket. A laptop that reconnects a few dozen times over a workday accumulates hundreds of ESTABLISHED connections to remote sites and eventually hits the per-process fd limit; the user sees "tunnel client error; retrying in 2s" loops with no explanation.

**Suggested fix:** Use a `JoinSet`/`CancellationToken` for the per-stream tasks and cancel them all when `run_once` exits; abort the writer on every exit path (or `select!` the writer join handle against the read loop); add a connect timeout (< `OPEN_TIMEOUT`) and check `streams` for a pending-cancel marker before inserting.

---

## [SEVERITY: medium] [CONFIDENCE: high] One slow TCP consumer head-of-line-blocks every other stream *and* all control frames

**Where:** `crates/box-egress-tunnel/src/server.rs:267-279`, `crates/box-egress-tunnel/src/client.rs:156-162`, per-stream channel depth 32 (`server.rs:348`, `client.rs:190`)

**What's wrong:** The WS read loop `await`s `tx.send(payload.to_vec())` on a bounded (32) per-stream channel while processing a single stream's frame. If that stream's consumer is not draining, the whole read loop parks — no other stream's data is read off the socket, and no control frame (`opened`, `error`, `close`) is processed either.

**Why it matters / how it fails:** A single stalled tab — a paused video, a download to a full disk, Chromium applying its own backpressure — freezes every other tab's traffic through the tunnel. Worse, because `opened` control frames are stuck behind the blocked data frame, *unrelated* new CONNECTs hit `OPEN_TIMEOUT` and return `504 client did not open the stream`, which reads like a laptop failure rather than internal congestion. This also amplifies a slow-laptop scenario into an apparent total outage.

**Suggested fix:** Give control frames their own path (process `Text` frames without ever blocking on a data channel), and use `try_send` with a per-stream "drop and close this stream" policy, or spawn a per-stream pump so a blocked stream cannot stall the shared reader.

---

## [SEVERITY: medium] [CONFIDENCE: medium] Unbounded per-frame memory: default tungstenite limits (64 MiB message / 16 MiB frame) plus copying and deep queues

**Where:** `crates/box-egress-tunnel/src/server.rs:172` (`accept_hdr_async` with no `WebSocketConfig`), `server.rs:222` (`mpsc::channel::<WsOut>(256)`), `server.rs:246`/`268` (`to_vec()` copies), `client.rs:104`/`108`

**What's wrong:** No `WebSocketConfig` is supplied anywhere, so tungstenite's defaults apply: `max_message_size` 64 MiB and `max_frame_size` 16 MiB. Every payload is copied at least twice (`decode_data(...).to_vec()`, then `encode_data` builds a new `Vec`). The outgoing queue holds 256 `WsOut` items of up to `CHUNK` (32 KiB) each — ~8 MiB — on top of whatever tungstenite has buffered.

**Why it matters / how it fails:** The guest runs under `mem_limit: 4g` shared with Chromium and Xvfb. A client holding the bearer (or a hijacker, see the cross-session finding) sends a single 64 MiB binary frame: the guest buffers 64 MiB inside tungstenite, copies it to a `Vec`, and tries to queue it; open a handful of WS connections and the box OOMs, taking Chromium and box-exec with it. Even benignly, a slow laptop uplink plus many active streams inflates guest RSS with no ceiling.

**Suggested fix:** Pass an explicit `WebSocketConfig` with `max_frame_size`/`max_message_size` around 128-256 KiB (frames are already chunked at 32 KiB), cap total in-flight bytes per session, and reuse buffers (`Bytes`) instead of `to_vec()`.

---

## [SEVERITY: medium] [CONFIDENCE: high] Bearer brute force against 8790 is unthrottled and invisible at the default log level

**Where:** `crates/box-egress-tunnel/src/server.rs:113-117` (`tracing::debug!` on session end), `server.rs:172-177` (401 path returns with no log), `RUST_LOG=info` default in `docker/Dockerfile:104`

**What's wrong:** A failed handshake produces an `ErrorResponse(401)`; `accept_hdr_async` then returns `Err`, which `handle_ws` propagates and `run_with_binds` logs at **debug** as "ws session ended". At the shipped `RUST_LOG=info` nothing is emitted. There is no per-peer rate limit, no failure counter, no lockout, and no cap on handshake attempts per second.

**Why it matters / how it fails:** Anything that can reach 8790 — the compose network, host loopback, or whoever the operator forwarded `-L 8790:` to — can grind the bearer at line rate, and `/tmp/box-egress-tunnel.log` will contain zero evidence. The consequence of success is not "read a status page", it is "become the exit node for the operator's browsing and gain the SSRF pivot in the first finding", so this deserves at least an audit trail.

**Suggested fix:** Log rejected handshakes at `warn` with the peer address and the header that was presented (never the value), add a simple per-IP failure counter with exponential delay, and mention in `docs/EGRESS.md` that 8790 must be reachable only through the SSH tunnel.

---

## [SEVERITY: medium] [CONFIDENCE: high] `probe_egress` does blocking socket I/O (up to ~600 ms) inside async axum handlers

**Where:** `crates/box-egress-tunnel/src/status.rs:102-129` (`std::net::TcpStream::connect_timeout` + `read_to_end`), called from `crates/box-host/src/lib.rs:231-236` (`/v1/info`) and `:257-259` (`/v1/egress`)

**What's wrong:** `fetch_admin` is synchronous `std::net` code with a 200 ms connect timeout and a 400 ms read timeout, called directly from `async fn info` / `async fn egress` on a tokio worker thread.

**Why it matters / how it fails:** Every `GET /v1/info` (which orchestrators poll) blocks a tokio worker for up to ~600 ms when the tunnel process is wedged or the admin socket accepts but does not answer — the worst case is exactly when things are broken. With the default worker count, a few concurrent `/v1/info` calls stall unrelated box-host requests, including `/v1/ready`, which is what the compose healthcheck hits — so a sick tunnel can make the container look unhealthy and get restarted. (`box-chrome::probe_chrome` has the same shape, so this is a pre-existing pattern being extended, not a new invention.)

**Suggested fix:** Make the probe async (`tokio::net::TcpStream` + `tokio::time::timeout`), or at minimum wrap it in `spawn_blocking`; better still, cache the last probe for ~1 s so `/v1/info` polling does not translate 1:1 into admin connections.

---

## [SEVERITY: medium] [CONFIDENCE: high] The tunnel bearer is only scrubbed from the environment when the tunnel is enabled

**Where:** `docker/entrypoint.sh:272-279` vs `docker/entrypoint.sh:25-37`

**What's wrong:** `BOX_TOKEN`/`BOX_HOST_TOKEN`/`BOX_VNC_PASSWORD` are read and `unset` unconditionally at the top of the entrypoint. `BOX_EGRESS_TUNNEL_BEARER` is only read and unset **inside `start_egress`**, which runs only when `BOX_EGRESS_TUNNEL` is on (`entrypoint.sh:319-321`).

**Why it matters / how it fails:** The natural operator workflow is to put the bearer in `.env` once and flip `BOX_EGRESS_TUNNEL` between 0 and 1. With `BOX_EGRESS_TUNNEL=0` and the value form, `BOX_EGRESS_TUNNEL_BEARER` stays exported and is inherited by every child the entrypoint spawns — Xvfb, xfce4-session, x11vnc, websockify, box-exec, box-host — so it is a `cat /proc/<any pid>/environ` away for anything running as uid 1000 inside the box, which is precisely what the surrounding comment says must not happen. (`box-exec` does strip it from exec children and `wipe_secret_environ` clears its own view, so the exposure is via the sibling daemons' environ blocks, not via `POST /v1/exec` env.)

**Suggested fix:** Read and `unset` `BOX_EGRESS_TUNNEL_BEARER` next to the other secrets at the top of the file, unconditionally, and pass the staged path into `start_egress`.

---

## [SEVERITY: medium] [CONFIDENCE: high] The allowlist matches the CONNECT string, not the resolved address, and ignores the port entirely

**Where:** `crates/box-egress-tunnel/src/allowlist.rs:1-9` (doc comment states this), `allowlist.rs:52-64`, `crates/box-egress-tunnel/src/client.rs:187`

**What's wrong:** Even when an operator does set `BOX_EGRESS_RELAY_HOSTS`, the check is a lowercase string comparison against the CONNECT authority, the client re-resolves the name at dial time, and the port is never considered.

**Why it matters / how it fails:** (a) The allowlist constrains a *name*, but the name's owner controls where it points. An operator sets `BOX_EGRESS_RELAY_HOSTS=*.somecdn.example` (a suffix with customer-controlled subdomains, which is the normal shape of a CDN allowlist); an attacker who can register `evil.somecdn.example` points its A record at `192.168.1.1` or `127.0.0.1` and the laptop client dials it, because `allows()` only saw the string and `TcpStream::connect((host, port))` re-resolves at dial time. The same re-resolution makes classic DNS rebinding work even for a name that resolved to a public IP when the allowlist author checked it. (b) Port: `CONNECT allowed.example:22` passes the host check, so an allowlist intended to permit web traffic also permits SSH, database and admin ports on those hosts.

**Suggested fix:** Resolve on the client, apply an address-class filter to the resolved IPs, dial the vetted `SocketAddr`, and add a port allowlist (default `80,443`).

---

## [SEVERITY: medium] [CONFIDENCE: medium] `std::env::set_var` / `remove_var` are called from inside a running multi-threaded tokio runtime

**Where:** `crates/box-egress-tunnel/src/config.rs:134-135` (`load_bearer`), `config.rs:64` and `config.rs:90` (`wipe_secret_environ`), reached from `crates/box-egress-tunnel/src/main.rs:91`/`117` under `#[tokio::main]`

**What's wrong:** `load_bearer` mutates the process environment (`set_var` then `remove_var`) and `wipe_secret_environ` removes six variables — all after `#[tokio::main]` has already started the multi-threaded runtime and its worker threads.

**Why it matters / how it fails:** `std::env::set_var`/`remove_var` are documented as not thread-safe; concurrent `getenv` from another thread (tokio, tracing, the TLS stack reading `SSL_CERT_FILE`, any `libc` call that reads the environment) can read freed memory. It is UB today under edition 2021 and a hard compile error once this crate moves to edition 2024, where both functions are `unsafe`. The realistic failure is a rare crash at startup, which would be near-impossible to diagnose.

**Suggested fix:** Do not route the file path through the environment — add a `read_secret_from_path(path, unlink)` in `box-common` and call it directly. Do the environment wipe once, before the runtime starts (e.g. in a non-async `fn main` that then builds the runtime).

---

## [SEVERITY: medium] [CONFIDENCE: medium] `--bearer` / `BOX_EGRESS_TUNNEL_BEARER` exposure: argv in `ps`, and clap prints the env value in `--help`

**Where:** `crates/box-egress-tunnel/src/main.rs:32-33` (server) and `main.rs:45-46` (client), `Cargo.toml:46` (`clap` with the `env` feature)

**What's wrong:** Both subcommands accept the secret as `--bearer <VALUE>` and via `env = "BOX_EGRESS_TUNNEL_BEARER"`. Clap's `hide_env_values` defaults to false, so `--help` renders `[env: BOX_EGRESS_TUNNEL_BEARER=<the actual value>]` when the variable is set.

**Why it matters / how it fails:** `box-egress-tunnel client --bearer s3cr3t...` puts the token in `/proc/<pid>/cmdline`, readable by every process of that uid — on the *operator's laptop*, where other user processes are far less constrained than in the box. And on the guest side, `BOX_EGRESS_TUNNEL_BEARER=... box-egress-tunnel server --help` prints the secret to stdout/logs. The entrypoint avoids both (it uses `--bearer-file` with a staged copy), so this is about the documented manual/Bot-spawn path.

**Suggested fix:** `.hide_env_values(true)` on both bearer args; consider removing the `--bearer` value flag entirely (file or env only) and noting the `ps` exposure in `docs/EGRESS.md`'s flag table, which currently only says "Avoid; prefer file".

---

## [SEVERITY: low] [CONFIDENCE: high] The server unlinks whatever `--bearer-file` points at, not only a staged copy

**Where:** `crates/box-egress-tunnel/src/config.rs:128-138` (`load_bearer(..., unlink_file = true)`), `crates/box-egress-tunnel/src/main.rs:91`

**What's wrong:** The server path always passes `unlink_file = true`, and `bearer_file` also picks up `BOX_EGRESS_TUNNEL_BEARER_FILE` from the environment via clap. `docs/EGRESS.md:99` describes this as unlinking "a **staged** copy", but the code deletes any path it is handed.

**Why it matters / how it fails:** An operator running the server outside the entrypoint (`BOX_EGRESS_TUNNEL_BEARER_FILE=~/secrets/egress box-egress-tunnel server`) loses the file. Inside the container against a compose/k8s secret mount the `remove_file` fails with `EBUSY`/`EROFS` and only warns, so the damage is limited there — but the native/dev path (`scripts/smoke-native.sh` territory) has no such protection.

**Suggested fix:** Only unlink when the caller explicitly asks (`--unlink-bearer-file`, set by the entrypoint), or refuse to unlink a path outside a staged tmp dir.

---

## [SEVERITY: low] [CONFIDENCE: high] Admin listener: no read timeout, single-read request matching, unbounded connections

**Where:** `crates/box-egress-tunnel/src/server.rs:139-164`

**What's wrong:** `handle_admin` does one `stream.read(&mut buf)` with no timeout and matches on `req.starts_with("GET /v1/health")`. A connection that never writes parks a task and an fd forever; a request split across TCP segments (`GET` then `/v1/status`) returns 404; there is no `Content-Length`/body handling and no connection cap. It is unauthenticated by design (loopback-only), which is stated in `docs/EGRESS.md:96`.

**Why it matters / how it fails:** Any process in the guest — including anything the agent runs via `POST /v1/exec` — can `nc 127.0.0.1 8792` in a loop and exhaust the tunnel's fds, which (combined with the `accept()` finding) takes down the box. The 404-on-split-read case is benign but would produce a confusing intermittent "enabled, not ready" from box-host's probe.

**Suggested fix:** `tokio::time::timeout` around the read, loop until `\r\n\r\n`, and cap concurrent admin connections (a semaphore of 4 is plenty).

---

## [SEVERITY: low] [CONFIDENCE: high] No timeout on the CONNECT proxy header read (in-guest slow loris)

**Where:** `crates/box-egress-tunnel/src/http1.rs:28-47`

**What's wrong:** `read_http_head` loops on `stream.read` with no deadline; the 16 KiB `MAX_HEAD` cap is checked *before* each read, so a client dribbling one byte per minute holds a task and an fd indefinitely.

**Why it matters / how it fails:** Reachable only from inside the guest (the proxy is loopback by default), so the attacker is the agent or page-driven code that can open raw sockets — realistically, an in-guest process. Same fd-exhaustion chain as above.

**Suggested fix:** Wrap `read_http_head` in a 10 s `timeout` and enforce `MAX_HEAD` after appending.

---

## [SEVERITY: low] [CONFIDENCE: medium] Hand-rolled head parsing: bare-LF header lines forwarded verbatim, folded headers abort the scan, hop-by-hop headers kept

**Where:** `crates/box-egress-tunnel/src/http1.rs:90-101` (`header_value`), `http1.rs:120-140` (`rewrite_origin_form`), `http1.rs:23-26` (`split_head`)

**What's wrong:** (a) The head is split on `\r\n` only, so a line containing a bare `\n` is treated as one "header line" and copied to the upstream byte-for-byte — a classic smuggling primitive for any upstream that accepts bare LF as a terminator. (b) `header_value` uses `line.split_once(':')?`, which returns from the *whole function* on the first line without a colon (e.g. an obs-fold continuation), so a `Host:` header after a folded line is never found and the request 400s. (c) Only `Proxy-Connection` is stripped; `Connection`, `Keep-Alive`, `TE`, `Upgrade`, `Transfer-Encoding` and `Proxy-Authorization` are forwarded. (d) `split_head` does not accept `\n\n` as a head terminator. The request line itself cannot be CRLF-injected (fields come from `split_whitespace`), which is the good news.

**Why it matters / how it fails:** Concretely: an in-guest process sends `GET http://a.example/ HTTP/1.1\r\nX: 1\nContent-Length: 44\r\nHost: a.example\r\n\r\n` and the upstream (or an intermediary CDN) sees a second header, desyncing the connection. Severity is limited because each proxied request gets its own fresh upstream TCP connection and the attacker already has in-guest code execution (they could speak to the origin directly via CONNECT) — the real exposure is against upstream intermediaries the guest could not otherwise reach the same way.

**Suggested fix:** Reject any head containing a bare `\n` or `\r` not part of a `\r\n`; skip (rather than abort on) malformed header lines; strip the full hop-by-hop set; ideally use `httparse` instead of hand-rolling.

---

## [SEVERITY: low] [CONFIDENCE: high] 503 body says "no client attached" when the real cause is stream-limit exhaustion

**Where:** `crates/box-egress-tunnel/src/server.rs:346` (`session.streams.len() < MAX_STREAMS`) → `server.rs:361-370`

**What's wrong:** Both "no session" and "`MAX_STREAMS` (256) reached" fall into the same `None` branch and return the same body, `"box-egress-tunnel: no client attached\n"`. `docs/EGRESS.md:195` even documents `503` for both causes without distinguishing them.

**Why it matters / how it fails:** The operator sees "no client attached" in Chromium's error page while `/v1/egress` simultaneously says `client_attached: true`. The two honest signals contradict each other and the actionable advice (fewer concurrent connections / restart the tunnel) is nowhere in the message.

**Suggested fix:** Separate the two branches; return `503 "no client attached"` vs `503 "tunnel stream limit reached (256)"`, and expose the current stream count in `/v1/egress` so the operator can see it coming.

---

## [SEVERITY: low] [CONFIDENCE: high] `docker/box-chromium` builds proxy flags as unquoted strings instead of an array

**Where:** `docker/box-chromium:6-17`, used at `:29-30`

**What's wrong:** `proxy_server` / `proxy_bypass` are plain strings expanded unquoted (`${proxy_server} ${proxy_bypass}`) so that an empty value disappears. The entrypoint does the same job correctly with a bash array (`entrypoint.sh:233-240`).

**Why it matters / how it fails:** The expansion is subject to word splitting and pathname expansion; `--proxy-bypass-list=localhost;127.0.0.1;[::1];<-loopback>` contains `[` `]` glob metacharacters, so in the (admittedly unlikely) case that a matching filename exists in the launcher's CWD the flag is silently rewritten. More practically it is a divergence between two copies of the same logic that will drift — the entrypoint already gained flags the wrapper does not have.

**Suggested fix:** Use `proxy_args=()` + `"${proxy_args[@]}"`, and factor the flag list into one shared snippet sourced by both.

---

## [SEVERITY: low] [CONFIDENCE: medium] Reconnect logic keys off substring matching on the formatted error, with a fixed 2 s backoff

**Where:** `crates/box-egress-tunnel/src/client.rs:62-85`

**What's wrong:** `is_unauthorized` does `format!("{err:#}").to_ascii_lowercase()` and looks for `"401"` or `"unauthorized"`. The reconnect delay is a hard-coded `Duration::from_secs(2)` with no backoff, jitter or attempt cap.

**Why it matters / how it fails:** False positive: a connection failure to, say, `ws://10.0.0.5:8401` or an OS error whose text contains "401" is misread as an auth failure and the client gives up permanently — the operator sees the tunnel "not reconnecting" with a wrong reason. False negative: a reverse proxy in front of 8790 that answers 403, or returns an HTML body whose Display text lacks those tokens, makes the client hammer the box every 2 s forever. The relay_tests only assert on the error *text* (`relay_tests.rs:188-192`), so this coupling is untested behaviourally.

**Suggested fix:** Match on the structured error (`tungstenite::Error::Http(resp)` → `resp.status()`) and treat 401/403 as fatal; add exponential backoff with jitter capped at ~30 s.

---

## [SEVERITY: low] [CONFIDENCE: high] Every CONNECT destination is logged at `info` — the container log becomes a browsing history

**Where:** `crates/box-egress-tunnel/src/server.rs:422-428`, also `server.rs:338` for rejects and `client.rs:200` on the laptop

**What's wrong:** `tracing::info!(host, port, id, connect, "relaying CONNECT")` fires per connection, and the shipped default is `RUST_LOG=info`, writing to `/tmp/box-egress-tunnel.log` in the guest and to the operator's terminal on the laptop.

**Why it matters / how it fails:** The whole point of the feature is logging into personal accounts (the docs name Facebook). Every hostname the agent's browser touches — including the operator's own accounts — is written to a file readable by anything running as uid 1000 in the box, and the box is the thing whose compromise you are guarding against. It is also a privacy surprise for the operator whose laptop terminal now prints their agent's browsing.

**Suggested fix:** Demote per-connection destination logs to `debug`, keep an aggregate counter at `info`, and say in `docs/EGRESS.md` what is logged where.

---

## [SEVERITY: low] [CONFIDENCE: high] Non-loopback binds for the CONNECT proxy and admin are only a warning; the proxy has no authentication at all

**Where:** `crates/box-egress-tunnel/src/config.rs:50-61`, `docker-compose.yml:46-47` (both binds are operator-overridable)

**What's wrong:** `TunnelServerConfig::from_env` logs `tracing::warn!` if `proxy_bind`/`admin_bind` are not loopback and proceeds. `main.rs` (the path the entrypoint actually uses) does not even do that — the CLI `Server` arm never calls `from_env`, so the warning never fires in the container. The CONNECT proxy has no bearer, no allowlist of source addresses, nothing.

**Why it matters / how it fails:** One `.env` line (`BOX_EGRESS_PROXY_BIND=0.0.0.0:8791`) turns the box into an unauthenticated open proxy to the operator's home network for every container on the compose network — no `ports:` entry required, so the "we only publish 8790" review check does not catch it. The related `ensure_token_strength` call only consults `ws_bind.ip().is_loopback()` (`config.rs:45-49`, `main.rs:92-96`), so a loopback WS bind also unlocks a weak bearer under `BOX_ALLOW_INSECURE_DEV=1` regardless of how wide the proxy is bound.

**Suggested fix:** Refuse to start when `proxy_bind`/`admin_bind` are non-loopback unless an explicit `--i-know-what-im-doing` flag is passed; move the check into the CLI path so it actually runs in the container; include all three binds in the loopback decision for token strength.

---

## [SEVERITY: low] [CONFIDENCE: high] No half-close propagation; a Chromium EOF tears down the remote direction immediately

**Where:** `crates/box-egress-tunnel/src/server.rs:474-508`, `crates/box-egress-tunnel/src/client.rs:220-254`

**What's wrong:** Both copy loops `break` on `Ok(0)` from their local socket and then send `close`, so a TCP half-close in either direction is converted into a full teardown. There is no `shutdown(Write)` propagation and no `Fin`/`half_close` control message in the protocol. Queued data already in the channel is delivered (the `Sender` drop drains first), so this is not a truncation bug in the common case.

**Why it matters / how it fails:** Protocols that half-close to signal end-of-request and then read a response (some upload flows, `gRPC`-over-TCP style patterns, an SMTP/IMAP tunnel if anyone points one at this proxy) lose the response. Browsers rarely half-close, so the practical impact for the stated Chromium use case is small — but the protocol has no way to express it, which is worth fixing in `box-egress-v1` now rather than after it has other consumers.

**Suggested fix:** Add `{"type":"shutdown","id":N,"dir":"write"}` to the wire and call `AsyncWriteExt::shutdown` on the corresponding socket instead of tearing down.

---

## [SEVERITY: low] [CONFIDENCE: high] Test gaps: no detach-mid-stream, no second-client, no non-CONNECT, no external-reachability test

**Where:** `crates/box-egress-tunnel/src/relay_tests.rs` (4 tests), `scripts/smoke.sh:174-181`, `scripts/smoke-native.sh:96-104`

**What's wrong:** The tests do cover the three things one would expect to be missing (503 fail-closed, 401 wrong bearer, 403 allowlist) — credit where due. What is not covered: (a) client attaches, a CONNECT is relaying, client dies — does `ready` drop and does the in-flight stream close promptly? (b) two clients attached at once (would have caught the cross-session injection finding); (c) the absolute-form/non-CONNECT path end-to-end (only `parse_proxy_request` is unit-tested, so the keep-alive misrouting finding is invisible); (d) an assertion that 8791/8792 are not bound on a non-loopback address; (e) slot cleanup after an error path (would have caught the `?` leak). The shell smokes only assert the capability keys exist and that `enabled` is false — they never exercise an enabled tunnel at all.

**Why it matters / how it fails:** The two properties the PR is selling — fail-closed and "one client at a time" — are each tested in exactly one narrow shape, so the regressions that matter (stale `ready`, takeover, slot leak) would ship silently.

**Suggested fix:** Add the detach-mid-stream and two-client tests (both are ~20 lines using the existing `start_tunnel` helper), plus a `bind_is_loopback` assertion test, and give `scripts/smoke.sh` an opt-in `BOX_EGRESS_TUNNEL=1` path that asserts a 503 from 127.0.0.1:8791 with no client attached.

---

## [SEVERITY: low] [CONFIDENCE: high] `docs/EGRESS.md` inaccuracies and a missing threat model

**Where:** `docs/EGRESS.md:193` ("One client at a time…"), `:100` (`BOX_EGRESS_RELAY_HOSTS` "empty = allow all"), `:195-197` (503 causes), `:206-210` (manual smoke), whole document (no threat model section)

**What's wrong:** Verified claim by claim, most of the document is accurate (the capability semantics, the bearer-strength rule, the `X-Box-Egress-Bearer` fallback, "no query-string token", the port table, the compose publish claim). Three things do not hold: (a) "One client at a time. A new successful handshake replaces the previous" — the map entry is replaced but the previous connection stays live and retains write access to the new session's streams; (b) `503` is documented for "no client / too many streams" but both emit the same misleading body; (c) the document never states that enabling this feature lets guest browser content reach the operator's LAN, laptop localhost and link-local addresses, and that the shipped default allowlist is "everything" — for a feature whose entire purpose is routing an agent's traffic through a human's home connection, that omission is the biggest documentation problem here. The "what is and is not tunneled" table (`:26-31`) is admirably honest and should be extended with a "what an attacker on the guest can reach" row.

**Suggested fix:** Fix (a) and (b) to match the code (or fix the code), and add a short "Threat model" section: who can reach 8790, what a compromised guest/page can do with the proxy, what the laptop client will and will not dial, and the recommendation to always set `BOX_EGRESS_RELAY_HOSTS`.

---

## [SEVERITY: low] [CONFIDENCE: high] README diff introduces two duplicated table rows

**Where:** `README.md:16-17` (`CLI buwiz-computer` row duplicated) and `README.md:173-174` (`BOX_MAX_CONCURRENT_EXECS` row duplicated)

**What's wrong:** Both rows now appear twice verbatim in the rendered tables.

**Why it matters / how it fails:** Cosmetic, but it is the first thing a reader of this PR sees, and it suggests the env-table block was pasted rather than merged — worth a second look at the surrounding rows for other paste artifacts.

**Suggested fix:** Delete the two duplicate lines.

---

## Categories that turned up nothing

- **Constant-time auth / empty-token-means-no-auth:** clean. `tokens_equal` is `subtle`-based with a length-mismatch dummy compare, `authorized()` requires a credential on both accepted header forms, and every construction path rejects an empty bearer before the listener starts.
- **Compose port publication:** clean. Grepped every compose/Dockerfile in the tree: 8791 and 8792 appear only as `127.0.0.1` binds and as "never publish" documentation; only `127.0.0.1:8790:8790` is published, and `EXPOSE 8790` does not publish anything.
- **Token in argv/logs on the shipped entrypoint path:** clean. The entrypoint stages a `0400` copy in a `0700` mktemp dir, passes `--bearer-file`, and the daemon unlinks it; the bearer is never interpolated into a log line. (The `--bearer` value flag and the clap `--help` echo are separate findings above, and neither is on the entrypoint path.)
- **Entrypoint ordering:** clean. `start_egress` runs before `start_chrome` and blocks on a real readiness poll of `/v1/status`, failing the container start if the tunnel never answers — not a `sleep`.
