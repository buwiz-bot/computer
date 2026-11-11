//! Guest server: the HTTP CONNECT proxy for Chromium, the tunnel's WebSocket, and the loopback
//! admin port.
//!
//! Every CONNECT is judged by the computer's rules first (`box_egress::rules`). No rules is no
//! browsing. Then the rules' exit decides who dials: the box itself (`direct`, every resolved
//! address judged by the address policy, with a DNS timeout), or the operator's client over the
//! WebSocket (`tunnel`, which judges the name and the address again on its side). With the tunnel
//! as the exit and no client attached the proxy answers 503, and never falls back to dialling
//! itself.
//!
//! Every refusal carries `X-Box-Egress-Reason`, one stable word, so "no client" and "too many
//! streams" (both 503) are told apart. No log line, status or reply names a site.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{header, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async_with_config, WebSocketStream};

use box_common::{parse_bearer, tokens_equal};
use box_egress::dial::{resolve_and_dial, DialError};
use box_egress::rules::{Exit, RulesSummary, Verdict};

use crate::config::{read_rules, TunnelServerConfig};
use crate::http1::{parse_proxy_request, read_http_head};
use crate::protocol::{
    decode_data, encode_data, ControlMsg, CHUNK, KEEPALIVE_INTERVAL, KEEPALIVE_MISSES, MAX_FRAME,
    MAX_MESSAGE, OPEN_TIMEOUT, PROTOCOL_NAME,
};
use crate::status::{format_bind, Counts, EgressStatus};

/// Streams at once through the tunnel, and, separately, dialled directly.
const MAX_STREAMS: usize = 256;
/// Proxy connections at once, in any state: every stream, and some to spare.
const MAX_PROXY_CONNECTIONS: usize = 2 * MAX_STREAMS + 64;
/// Proxy connections whose request head hasn't come yet, and how long one has to send it. Any
/// process in the box can reach the proxy: idle or slow sockets go, and can't crowd the browser out.
const MAX_UNREAD: usize = 64;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

enum WsOut {
    Control(ControlMsg),
    Data { id: u32, payload: Vec<u8> },
    Ping,
}

/// A peer that opens a socket and never sends the upgrade used to pin a task
/// and a file descriptor forever, with no bearer needed — auth happens after
/// accept. Bound both the wait and how many may wait at once.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_HANDSHAKES: usize = 64;
/// How long a handshake with a wrong or missing bearer is held before its 401. With at most
/// [`MAX_PENDING_HANDSHAKES`] waiting, that bounds guessing at 64 a second, against a bearer of 32
/// random bytes; the published port is loopback on the operator's machine.
const AUTH_FAIL_DELAY: Duration = Duration::from_secs(1);
/// The request head of an upgrade, at most.
const MAX_UPGRADE_HEAD: usize = 16 * 1024;

/// Frame and message caps for both directions. The defaults (16 MiB per frame,
/// 64 MiB per message) are sized for a server with headroom, not a 4 GB guest
/// relaying byte streams that are chunked at the TCP layer anyway.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_FRAME))
}

/// One accept failure must not end the listener: `ECONNABORTED` is routine, and
/// `EMFILE` under a flood of idle sockets used to `?` out of the loop, exit the
/// process, and take the whole container down with it via the entrypoint's
/// death-watch. Log, back off briefly on resource exhaustion, carry on.
async fn accept_or_skip<T>(listener: &'static str, accepted: std::io::Result<T>) -> Option<T> {
    match accepted {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::warn!(listener, error = %err, "accept failed; continuing");
            if matches!(
                err.raw_os_error(),
                Some(libc_emfile) if libc_emfile == 24 || libc_emfile == 23
            ) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            None
        }
    }
}

struct StreamSlot {
    from_client: mpsc::Sender<Vec<u8>>,
    opened: Option<oneshot::Sender<Result<(), String>>>,
}

struct ClientSession {
    generation: u64,
    /// Fired by a newer attach so this session's reader loop stops. A second
    /// bearer holder used to overwrite the slot silently while the incumbent
    /// kept reading, and frames from either connection landed on whichever
    /// session was current.
    evict: Arc<Notify>,
    outgoing: mpsc::Sender<WsOut>,
    streams: HashMap<u32, StreamSlot>,
    next_id: u32,
}

#[derive(Default)]
struct Counters {
    direct: AtomicU64,
    tunneled: AtomicU64,
    blocked: AtomicU64,
    not_listed: AtomicU64,
    no_rules: AtomicU64,
    no_client: AtomicU64,
    streams_full: AtomicU64,
    destination: AtomicU64,
    failed: AtomicU64,
    plain_http: AtomicU64,
    bad_request: AtomicU64,
    bad_host: AtomicU64,
    relay_hosts: AtomicU64,
    busy: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Counts {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Counts {
            direct: get(&self.direct),
            tunneled: get(&self.tunneled),
            blocked: get(&self.blocked),
            not_listed: get(&self.not_listed),
            no_rules: get(&self.no_rules),
            no_client: get(&self.no_client),
            streams_full: get(&self.streams_full),
            destination: get(&self.destination),
            failed: get(&self.failed),
            plain_http: get(&self.plain_http),
            bad_request: get(&self.bad_request),
            bad_host: get(&self.bad_host),
            relay_hosts: get(&self.relay_hosts),
            busy: get(&self.busy),
        }
    }
}

struct Shared {
    cfg: TunnelServerConfig,
    handshakes: Semaphore,
    /// wrong bearers being held before their 401: apart from `handshakes`, so they never hold a
    /// right one out
    refusing: Semaphore,
    connections: Arc<Semaphore>,
    unread: Semaphore,
    direct_slots: Semaphore,
    attached: AtomicBool,
    generation: AtomicU64,
    session: Mutex<Option<ClientSession>>,
    ws_addr: Option<std::net::SocketAddr>,
    proxy_addr: std::net::SocketAddr,
    counts: Counters,
    rules_error: Mutex<Option<String>>,
    auth_failures: AtomicU64,
}

impl Shared {
    fn snapshot(&self) -> EgressStatus {
        let attached = self.attached.load(Ordering::SeqCst);
        let rules = self.cfg.rules.get().map(|r| RulesSummary::from(&*r));
        EgressStatus {
            enabled: self.cfg.tunnel.is_some(),
            ready: attached,
            client_attached: attached,
            protocol: PROTOCOL_NAME.to_string(),
            ws: self.ws_addr.map(format_bind).unwrap_or_default(),
            proxy: format_bind(self.proxy_addr),
            proxy_enabled: true,
            proxy_up: true,
            can_browse: EgressStatus::browsable(rules.as_ref(), attached),
            rules,
            rules_origin: None,
            rules_error: self
                .rules_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            counts: self.counts.snapshot(),
        }
    }

    /// Read the rules file again. A missing, unreadable or malformed file leaves no rules, which
    /// refuses every site: a set that didn't read is never kept in force by mistake.
    fn reload(&self) -> Result<Option<u64>, String> {
        let Some(path) = &self.cfg.rules_path else {
            return Err("this proxy has no rules file".into());
        };
        let read = read_rules(path);
        let mut error = self.rules_error.lock().unwrap_or_else(|e| e.into_inner());
        match read {
            Ok(rules) => {
                let version = rules.as_ref().map(|r| r.version);
                self.cfg.rules.set(rules);
                *error = None;
                match version {
                    Some(version) => tracing::info!(version, "egress rules loaded"),
                    None => tracing::warn!("no egress rules are set; every site is refused"),
                }
                Ok(version)
            }
            Err(why) => {
                self.cfg.rules.set(None);
                tracing::warn!(error = %why, "egress rules unreadable; every site is refused");
                *error = Some(why.clone());
                Err(why)
            }
        }
    }
}

pub struct ServerBinds {
    pub ws: Option<TcpListener>,
    pub proxy: TcpListener,
    pub admin: TcpListener,
}

impl ServerBinds {
    pub async fn bind(cfg: &TunnelServerConfig) -> std::io::Result<Self> {
        let ws = match &cfg.tunnel {
            Some(side) => Some(TcpListener::bind(side.bind).await?),
            None => None,
        };
        let proxy = TcpListener::bind(cfg.proxy_bind).await?;
        let admin = TcpListener::bind(cfg.admin_bind).await?;
        Ok(Self { ws, proxy, admin })
    }
}

pub async fn run(cfg: TunnelServerConfig) -> anyhow::Result<()> {
    let binds = ServerBinds::bind(&cfg).await?;
    run_with_binds(cfg, binds).await
}

async fn accept_ws(ws: &Option<TcpListener>) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
    match ws {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

pub async fn run_with_binds(cfg: TunnelServerConfig, binds: ServerBinds) -> anyhow::Result<()> {
    let ws_addr = binds.ws.as_ref().map(|l| l.local_addr()).transpose()?;
    let proxy_addr = binds.proxy.local_addr()?;
    let admin_addr = binds.admin.local_addr()?;
    tracing::info!(
        ?ws_addr,
        %proxy_addr,
        %admin_addr,
        protocol = PROTOCOL_NAME,
        "box-egress-tunnel server listening"
    );
    let shared = Arc::new(Shared {
        cfg,
        handshakes: Semaphore::new(MAX_PENDING_HANDSHAKES),
        refusing: Semaphore::new(MAX_PENDING_HANDSHAKES),
        connections: Arc::new(Semaphore::new(MAX_PROXY_CONNECTIONS)),
        unread: Semaphore::new(MAX_UNREAD),
        direct_slots: Semaphore::new(MAX_STREAMS),
        attached: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        session: Mutex::new(None),
        ws_addr,
        proxy_addr,
        counts: Counters::default(),
        rules_error: Mutex::new(None),
        auth_failures: AtomicU64::new(0),
    });
    if shared.cfg.rules_path.is_some() {
        let _ = shared.reload();
    } else if shared.cfg.rules.get().is_none() {
        tracing::warn!("no egress rules are set; every site is refused");
    }

    loop {
        tokio::select! {
            accepted = accept_ws(&binds.ws) => {
                let Some((stream, peer)) = accept_or_skip("ws", accepted).await else { continue };
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_ws(shared, stream, peer).await {
                        tracing::debug!(error = %err, %peer, "ws session ended");
                    }
                });
            }
            accepted = binds.proxy.accept() => {
                let Some((mut stream, _)) = accept_or_skip("proxy", accepted).await else { continue };
                let shared = shared.clone();
                let Ok(held) = shared.connections.clone().try_acquire_owned() else {
                    tokio::spawn(async move { refuse(&shared, &mut stream, &BUSY).await });
                    continue;
                };
                tokio::spawn(async move {
                    let _held = held;
                    if let Err(err) = handle_proxy(shared, stream).await {
                        tracing::debug!(error = %err, "proxy session ended");
                    }
                });
            }
            accepted = binds.admin.accept() => {
                let Some((stream, _)) = accept_or_skip("admin", accepted).await else { continue };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = handle_admin(shared, stream).await;
                });
            }
        }
    }
}

/// Loopback only, no login, so nothing here can change what the proxy does except reading the
/// rules file again, which only box-host's user can write.
async fn handle_admin(shared: Arc<Shared>, mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .unwrap_or(Ok(0))?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let (status, body) = if req.starts_with("GET /v1/health") {
        (200, b"{\"status\":\"ok\"}".to_vec())
    } else if req.starts_with("GET /v1/status") {
        (
            200,
            serde_json::to_vec(&shared.snapshot()).unwrap_or_else(|_| b"{}".to_vec()),
        )
    } else if req.starts_with("GET /v1/rules") {
        // counts only: this port has no login, and any process in the box can ask it
        match shared.cfg.rules.get() {
            Some(rules) => (
                200,
                serde_json::to_vec(&RulesSummary::from(&*rules)).unwrap_or_default(),
            ),
            None => (404, b"{\"error\":\"no_rules\"}".to_vec()),
        }
    } else if req.starts_with("POST /v1/rules/reload") {
        match shared.reload() {
            Ok(Some(version)) => (200, format!("{{\"version\":{version}}}").into_bytes()),
            Ok(None) => (404, b"{\"error\":\"no_rules\"}".to_vec()),
            Err(why) => (
                422,
                serde_json::to_vec(&serde_json::json!({"error": "bad_rules", "message": why}))
                    .unwrap_or_default(),
            ),
        }
    } else {
        (404, b"{\"error\":\"not_found\"}".to_vec())
    };
    let reason = match status {
        200 => "OK",
        422 => "Unprocessable Entity",
        _ => "Not Found",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// The upgrade's head, read without taking it off the socket, so the handshake reads it again.
/// `None` when it doesn't come in time or is too large.
async fn peek_head(stream: &TcpStream) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; MAX_UPGRADE_HEAD];
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let n = tokio::time::timeout_at(deadline, stream.peek(&mut buf))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            return Some(buf[..end + 4].to_vec());
        }
        if n == buf.len() || tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn presented_bearer(head: &str) -> Option<&str> {
    let mut x_box = None;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("authorization") {
            if let Some(token) = parse_bearer(value.trim()) {
                return Some(token);
            }
        } else if name.eq_ignore_ascii_case("x-box-egress-bearer") {
            x_box = Some(value.trim()).filter(|s| !s.is_empty());
        }
    }
    x_box
}

// The handshake callback's error type is tungstenite's `ErrorResponse`, a whole HTTP response: its
// size is the crate's signature, not a choice made here.
#[allow(clippy::result_large_err)]
async fn handle_ws(
    shared: Arc<Shared>,
    mut stream: TcpStream,
    _peer: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let Some(side) = shared.cfg.tunnel.clone() else {
        return Ok(());
    };
    let bearer = side.bearer;
    let Ok(permit) = shared.handshakes.try_acquire() else {
        tracing::warn!("too many pending handshakes; dropping the connection");
        return Ok(());
    };
    // The bearer is checked here first, so a wrong one waits before it is told: guessing used to
    // be as fast as the loopback port answered, and left no line in the log.
    let Some(head) = peek_head(&stream).await else {
        return Ok(());
    };
    let head = String::from_utf8_lossy(&head).into_owned();
    if !presented_bearer(&head).is_some_and(|token| tokens_equal(token, &bearer)) {
        // the handshake's place goes now: a wrong bearer waiting out its delay never holds a right
        // one out. Those waiting are capped apart; past the cap one is closed with no answer
        drop(permit);
        let Ok(_refusing) = shared.refusing.try_acquire() else {
            return Ok(());
        };
        let failures = shared.auth_failures.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            failures,
            "egress tunnel handshake refused: wrong or missing bearer"
        );
        tokio::time::sleep(AUTH_FAIL_DELAY).await;
        let _ = stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nContent-Length: 13\r\nConnection: close\r\n\r\nunauthorized\n",
            )
            .await;
        return Ok(());
    }
    let handshake = accept_hdr_async_with_config(
        stream,
        |req: &Request, mut response: Response| {
            if !authorized(req, &bearer) {
                let mut resp = ErrorResponse::new(Some("unauthorized\n".to_string()));
                *resp.status_mut() = StatusCode::UNAUTHORIZED;
                return Err(resp);
            }
            if let Some(val) = req.headers().get("sec-websocket-protocol") {
                if let Ok(s) = val.to_str() {
                    if s.split(',').any(|p| p.trim() == PROTOCOL_NAME) {
                        response.headers_mut().insert(
                            header::SEC_WEBSOCKET_PROTOCOL,
                            HeaderValue::from_static(PROTOCOL_NAME),
                        );
                    }
                }
            }
            Ok(response)
        },
        Some(ws_config()),
    );
    let ws = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
        .await
        .map_err(|_| anyhow::anyhow!("handshake timed out"))??;
    // The permit covers the handshake only; a session is bounded separately
    // (one at a time, by eviction).
    drop(permit);

    tracing::info!("egress client attached");
    serve_client(shared, ws).await;
    tracing::info!("egress client detached");
    Ok(())
}

fn authorized(req: &Request, expected: &str) -> bool {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer)
        .or_else(|| {
            req.headers()
                .get("x-box-egress-bearer")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
    match presented {
        Some(token) => tokens_equal(token, expected),
        None => false,
    }
}

async fn serve_client<S>(shared: Arc<Shared>, ws: WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(256);
    let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let evict = Arc::new(Notify::new());
    {
        let mut g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
        let previous = g.replace(ClientSession {
            generation,
            evict: evict.clone(),
            outgoing: out_tx.clone(),
            streams: HashMap::new(),
            next_id: 0,
        });
        if let Some(previous) = previous {
            // Documented as "a new handshake replaces the previous"; now it
            // actually does. Loud on purpose: whoever operates this box should
            // see that a second holder of the bearer attached.
            tracing::warn!(
                evicted = previous.generation,
                by = generation,
                "egress client replaced by a newer attach"
            );
            previous.evict.notify_one();
        }
        shared.attached.store(true, Ordering::SeqCst);
    }

    let _ = out_tx
        .send(WsOut::Control(ControlMsg::hello("server")))
        .await;

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let frame = match msg {
                WsOut::Control(ctrl) => match serde_json::to_string(&ctrl) {
                    Ok(text) => Message::Text(text.into()),
                    Err(_) => continue,
                },
                WsOut::Data { id, payload } => Message::Binary(encode_data(id, &payload).into()),
                WsOut::Ping => Message::Ping(Vec::new().into()),
            };
            if sink.send(frame).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await; // the first tick is immediate; consume it
    let mut silent_ticks: u8 = 0;
    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(Ok(frame)) = frame else {
                    break;
                };
                // Any frame at all is proof of life, so a peer that only ever
                // answers our pings still counts.
                silent_ticks = 0;
                match frame {
                    Message::Text(text) => {
                        if let Ok(msg) = serde_json::from_str::<ControlMsg>(&text) {
                            if msg.wire_ok() {
                                on_control(&shared, generation, msg);
                            }
                        }
                    }
                    Message::Binary(bin) => {
                        if let Some((id, payload)) = decode_data(&bin) {
                            let tx = {
                                let g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
                                g.as_ref()
                                    // Only this connection's own session: a frame
                                    // from an evicted connection must not reach a
                                    // stream the new client opened under the same id.
                                    .filter(|s| s.generation == generation)
                                    .and_then(|s| s.streams.get(&id))
                                    .map(|slot| slot.from_client.clone())
                            };
                            if let Some(tx) = tx {
                                match tx.try_send(payload.to_vec()) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        // Awaiting here parked the whole reader --
                                        // every other stream and all control frames
                                        // -- behind one consumer that stopped
                                        // reading. Close that one stream instead.
                                        tracing::warn!(id, "stream consumer not keeping up; closing it");
                                        drop_stream(&shared, generation, id);
                                        let _ = out_tx
                                            .send(WsOut::Control(ControlMsg::close(
                                                id,
                                                Some("slow consumer"),
                                            )))
                                            .await;
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                                }
                            }
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => break,
                }
            }
            _ = evict.notified() => {
                tracing::info!(generation, "egress client session evicted");
                break;
            }
            _ = keepalive.tick() => {
                if silent_ticks >= KEEPALIVE_MISSES {
                    tracing::warn!(
                        generation,
                        silent_for_secs = KEEPALIVE_INTERVAL.as_secs() * u64::from(KEEPALIVE_MISSES),
                        "egress client silent; dropping session so `ready` tells the truth"
                    );
                    break;
                }
                silent_ticks += 1;
                if out_tx.send(WsOut::Ping).await.is_err() {
                    break;
                }
            }
        }
    }

    writer.abort();
    let mut g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|s| s.generation == generation) {
        *g = None;
        shared.attached.store(false, Ordering::SeqCst);
    }
}

fn on_control(shared: &Shared, generation: u64, msg: ControlMsg) {
    let Some(id) = msg.stream_id() else {
        return;
    };
    let mut g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = g.as_mut().filter(|s| s.generation == generation) else {
        // Not this connection's session any more: an evicted client could
        // otherwise `close` or fake-`opened` the new client's streams.
        return;
    };
    match msg {
        ControlMsg::Opened { .. } => {
            if let Some(slot) = session.streams.get_mut(&id) {
                if let Some(tx) = slot.opened.take() {
                    let _ = tx.send(Ok(()));
                }
            }
        }
        ControlMsg::Error { message, .. } => {
            if let Some(slot) = session.streams.get_mut(&id) {
                if let Some(tx) = slot.opened.take() {
                    let _ = tx.send(Err(message));
                }
            }
            session.streams.remove(&id);
        }
        ControlMsg::Close { .. } => {
            session.streams.remove(&id);
        }
        ControlMsg::Hello { .. } | ControlMsg::Open { .. } => {}
    }
}

/// Which count a refusal goes in: every refusal is counted.
#[derive(Clone, Copy)]
enum Counter {
    PlainHttp,
    BadRequest,
    NoRules,
    Blocked,
    NotListed,
    BadHost,
    RelayHosts,
    NoClient,
    StreamsFull,
    Busy,
    Destination,
    Failed,
}

/// A refusal: its status, the stable word for `X-Box-Egress-Reason`, a sentence, and its count.
struct Refusal {
    status: u16,
    reason: &'static str,
    text: &'static str,
    counter: Counter,
}

impl Refusal {
    const fn new(status: u16, reason: &'static str, text: &'static str, counter: Counter) -> Self {
        Self {
            status,
            reason,
            text,
            counter,
        }
    }
}

const PLAIN_HTTP: Refusal = Refusal::new(
    405,
    "plain-http",
    "this computer's browser loads https:// pages only; plain http:// isn't relayed",
    Counter::PlainHttp,
);
const BAD_REQUEST: Refusal = Refusal::new(400, "bad-request", "bad request", Counter::BadRequest);
const NO_RULES: Refusal = Refusal::new(
    403,
    "no-rules",
    "no browsing rules are set for this computer, so every site is refused",
    Counter::NoRules,
);
const BLOCKED: Refusal = Refusal::new(
    403,
    "blocked",
    "blocked by this computer's rules",
    Counter::Blocked,
);
const NOT_LISTED: Refusal = Refusal::new(
    403,
    "not-listed",
    "not on this computer's allow list, and it reaches listed sites only",
    Counter::NotListed,
);
const BAD_HOST: Refusal =
    Refusal::new(400, "bad-host", "that isn't a valid host", Counter::BadHost);
const RELAY_HOSTS: Refusal =
    Refusal::new(403, "relay-hosts", "host not allowed", Counter::RelayHosts);
const NO_CLIENT: Refusal = Refusal::new(
    503,
    "no-client",
    "box-egress-tunnel: no client attached; this computer browses through the tunnel only",
    Counter::NoClient,
);
const STREAMS_FULL: Refusal = Refusal::new(
    503,
    "streams-full",
    "box-egress-tunnel: too many streams at once",
    Counter::StreamsFull,
);
const DESTINATION: Refusal = Refusal::new(
    403,
    "destination",
    "destination refused: not a public address, or a port that isn't relayed",
    Counter::Destination,
);
const RESOLVE: Refusal = Refusal::new(502, "resolve", "the name didn't resolve", Counter::Failed);
const CONNECT: Refusal = Refusal::new(502, "connect", "the site didn't answer", Counter::Failed);
const TIMEOUT: Refusal = Refusal::new(
    504,
    "timeout",
    "the site didn't answer in time",
    Counter::Failed,
);
const SLOW_HEAD: Refusal = Refusal::new(
    408,
    "slow-request",
    "the request didn't come in time",
    Counter::BadRequest,
);
const BUSY: Refusal = Refusal::new(
    503,
    "busy",
    "box-egress-tunnel: too many connections waiting to be read",
    Counter::Busy,
);
const OPEN_TIMED_OUT: Refusal = Refusal::new(
    504,
    "open-timeout",
    "client did not open the stream",
    Counter::Failed,
);

async fn refuse(shared: &Shared, stream: &mut TcpStream, refusal: &Refusal) {
    let c = &shared.counts;
    Counters::bump(match refusal.counter {
        Counter::PlainHttp => &c.plain_http,
        Counter::BadRequest => &c.bad_request,
        Counter::NoRules => &c.no_rules,
        Counter::Blocked => &c.blocked,
        Counter::NotListed => &c.not_listed,
        Counter::BadHost => &c.bad_host,
        Counter::RelayHosts => &c.relay_hosts,
        Counter::NoClient => &c.no_client,
        Counter::StreamsFull => &c.streams_full,
        Counter::Busy => &c.busy,
        Counter::Destination => &c.destination,
        Counter::Failed => &c.failed,
    });
    tracing::info!(
        status = refusal.status,
        reason = refusal.reason,
        "CONNECT refused"
    );
    let _ = write_http(
        stream,
        refusal.status,
        refusal.reason,
        &format!("{}\n", refusal.text),
    )
    .await;
}

async fn handle_proxy(shared: Arc<Shared>, mut stream: TcpStream) -> anyhow::Result<()> {
    let Ok(unread) = shared.unread.try_acquire() else {
        refuse(&shared, &mut stream, &BUSY).await;
        return Ok(());
    };
    let read = tokio::time::timeout(HEAD_TIMEOUT, read_http_head(&mut stream)).await;
    drop(unread);
    let (head, rest) = match read {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            refuse(&shared, &mut stream, &BAD_REQUEST).await;
            return Ok(());
        }
        Err(_) => {
            refuse(&shared, &mut stream, &SLOW_HEAD).await;
            return Ok(());
        }
    };
    let Some(req) = parse_proxy_request(&head, &rest) else {
        refuse(&shared, &mut stream, &BAD_REQUEST).await;
        return Ok(());
    };

    if !req.connect {
        // CONNECT only, whichever the exit. Absolute-form proxying read one request head, opened
        // one upstream, then relayed every later byte on the client socket to that first
        // upstream -- and Chromium keys plain-http proxy sockets on the proxy, not the origin, so
        // a second site's request went to the first site, cookies and all.
        refuse(&shared, &mut stream, &PLAIN_HTTP).await;
        return Ok(());
    }

    let Some(rules) = shared.cfg.rules.get() else {
        refuse(&shared, &mut stream, &NO_RULES).await;
        return Ok(());
    };
    match rules.judge(&req.host) {
        Verdict::Allowed => {}
        Verdict::Blocked => {
            refuse(&shared, &mut stream, &BLOCKED).await;
            return Ok(());
        }
        Verdict::NotListed => {
            refuse(&shared, &mut stream, &NOT_LISTED).await;
            return Ok(());
        }
        Verdict::BadHost => {
            refuse(&shared, &mut stream, &BAD_HOST).await;
            return Ok(());
        }
    }
    if !shared.cfg.allowlist.allows(&req.host) {
        refuse(&shared, &mut stream, &RELAY_HOSTS).await;
        return Ok(());
    }

    match rules.exit {
        Exit::Direct => direct(&shared, stream, req.host, req.port, req.initial).await,
        Exit::Tunnel => tunnel(&shared, stream, req.host, req.port, req.initial).await,
    }
    Ok(())
}

/// Dial the site from the box, judging every address it resolved to first.
async fn direct(shared: &Shared, mut stream: TcpStream, host: String, port: u16, initial: Vec<u8>) {
    let Ok(_slot) = shared.direct_slots.try_acquire() else {
        refuse(shared, &mut stream, &STREAMS_FULL).await;
        return;
    };
    let mut upstream = match resolve_and_dial(&host, port, &shared.cfg.destination).await {
        Ok(upstream) => upstream,
        Err(err) => {
            let refusal = match &err {
                DialError::Refused(_) => &DESTINATION,
                DialError::Resolve => &RESOLVE,
                err if err.timed_out() => &TIMEOUT,
                DialError::Connect(_) => &CONNECT,
            };
            refuse(shared, &mut stream, refusal).await;
            return;
        }
    };
    if stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }
    Counters::bump(&shared.counts.direct);
    tracing::debug!("relaying CONNECT directly");
    let _ = stream.set_nodelay(true);
    let _ = upstream.set_nodelay(true);
    if !initial.is_empty() && upstream.write_all(&initial).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
}

/// Hand the stream to the operator's client. No client is a 503, never a direct dial.
async fn tunnel(shared: &Shared, mut stream: TcpStream, host: String, port: u16, initial: Vec<u8>) {
    let prepared = {
        let mut g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            None => Err(&NO_CLIENT),
            Some(session) if session.streams.len() >= MAX_STREAMS => Err(&STREAMS_FULL),
            Some(session) => match alloc_id(session) {
                None => Err(&STREAMS_FULL),
                Some(id) => {
                    let (from_tx, from_rx) = mpsc::channel(256);
                    let (opened_tx, opened_rx) = oneshot::channel();
                    session.streams.insert(
                        id,
                        StreamSlot {
                            from_client: from_tx,
                            opened: Some(opened_tx),
                        },
                    );
                    Ok((
                        id,
                        from_rx,
                        opened_rx,
                        session.outgoing.clone(),
                        session.generation,
                    ))
                }
            },
        }
    };
    let (id, from_client_rx, opened_rx, outgoing, generation) = match prepared {
        Ok(prepared) => prepared,
        Err(refusal) => {
            refuse(shared, &mut stream, refusal).await;
            return;
        }
    };

    if outgoing
        .send(WsOut::Control(ControlMsg::open(id, host, port)))
        .await
        .is_err()
    {
        drop_stream(shared, generation, id);
        refuse(shared, &mut stream, &NO_CLIENT).await;
        return;
    }

    let opened = tokio::time::timeout(OPEN_TIMEOUT, opened_rx).await;
    match opened {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(message))) => {
            drop_stream(shared, generation, id);
            Counters::bump(&shared.counts.failed);
            tracing::info!(status = 502, reason = "client-error", "CONNECT refused");
            // The client's own words: they name no site (box_egress::client).
            let _ = write_http(&mut stream, 502, "client-error", &format!("{message}\n")).await;
            return;
        }
        Ok(Err(_)) | Err(_) => {
            let _ = outgoing
                .send(WsOut::Control(ControlMsg::close(id, Some("open-timeout"))))
                .await;
            drop_stream(shared, generation, id);
            refuse(shared, &mut stream, &OPEN_TIMED_OUT).await;
            return;
        }
    }

    if let Err(err) = stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
    {
        // The client left between CONNECT and 200. A `?` here skipped the
        // cleanup below; 256 of those and every later CONNECT got a 503
        // blaming a missing client while `/v1/egress` still said ready.
        tracing::debug!(id, error = %err, "client left before 200; releasing the stream");
        let _ = outgoing
            .send(WsOut::Control(ControlMsg::close(id, Some("client gone"))))
            .await;
        drop_stream(shared, generation, id);
        return;
    }

    Counters::bump(&shared.counts.tunneled);
    tracing::debug!(id, "relaying CONNECT through the tunnel");
    let _ = stream.set_nodelay(true);
    copy_proxy_stream(stream, from_client_rx, outgoing, id, initial).await;
    drop_stream(shared, generation, id);
}

fn alloc_id(session: &mut ClientSession) -> Option<u32> {
    for _ in 0..MAX_STREAMS + 8 {
        session.next_id = session.next_id.wrapping_add(1);
        if session.next_id == 0 {
            continue;
        }
        if !session.streams.contains_key(&session.next_id) {
            return Some(session.next_id);
        }
    }
    None
}

fn drop_stream(shared: &Shared, generation: u64, id: u32) {
    let mut g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
    // Stream ids restart at 1 per session, so without this a straggler from a
    // dropped client could remove the *new* client's live stream 1.
    if let Some(session) = g.as_mut().filter(|s| s.generation == generation) {
        session.streams.remove(&id);
    }
}

async fn copy_proxy_stream(
    mut chromium: TcpStream,
    mut from_client: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<WsOut>,
    id: u32,
    initial: Vec<u8>,
) {
    if !initial.is_empty()
        && outgoing
            .send(WsOut::Data {
                id,
                payload: initial,
            })
            .await
            .is_err()
    {
        return;
    }
    let mut buf = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            n = chromium.read(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if outgoing
                            .send(WsOut::Data {
                                id,
                                payload: buf[..n].to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            msg = from_client.recv() => {
                match msg {
                    Some(data) => {
                        if chromium.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let _ = outgoing
        .send(WsOut::Control(ControlMsg::close(id, Some("eof"))))
        .await;
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain\r\nX-Box-Egress-Reason: {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason_phrase(status),
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bearer_is_read_from_either_header() {
        let head = "GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer abc\r\n\r\n";
        assert_eq!(presented_bearer(head), Some("abc"));
        let head = "GET / HTTP/1.1\r\nX-Box-Egress-Bearer:  def \r\n\r\n";
        assert_eq!(presented_bearer(head), Some("def"));
        let head = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(presented_bearer(head), None);
    }
}
