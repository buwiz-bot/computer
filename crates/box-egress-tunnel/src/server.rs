//! Guest tunnel server: WebSocket mux + HTTP CONNECT proxy + loopback admin.

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

use crate::config::TunnelServerConfig;
use crate::http1::{parse_proxy_request, read_http_head};
use crate::protocol::{decode_data, encode_data, ControlMsg, PROTOCOL_NAME};
use crate::status::{format_bind, EgressStatus};

pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STREAMS: usize = 256;
const CHUNK: usize = 32 * 1024;

enum WsOut {
    Control(ControlMsg),
    Data { id: u32, payload: Vec<u8> },
    Ping,
}

/// How often the server pings an idle client, and how many silent intervals
/// it tolerates before dropping the session. Without this, `attached` only
/// flipped on a clean FIN or an RST: a laptop that went to sleep stayed
/// `ready: true` for as long as TCP keepalive took to notice, which is hours.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_MISSES: u8 = 2;

/// A peer that opens a socket and never sends the upgrade used to pin a task
/// and a file descriptor forever, with no bearer needed — auth happens after
/// accept. Bound both the wait and how many may wait at once.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_HANDSHAKES: usize = 64;

/// Frame and message caps for both directions. The defaults (16 MiB per frame,
/// 64 MiB per message) are sized for a server with headroom, not a 4 GB guest
/// relaying byte streams that are chunked at the TCP layer anyway.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(256 << 10))
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

struct Shared {
    cfg: TunnelServerConfig,
    handshakes: Semaphore,
    attached: AtomicBool,
    generation: AtomicU64,
    session: Mutex<Option<ClientSession>>,
    ws_addr: std::net::SocketAddr,
    proxy_addr: std::net::SocketAddr,
}

impl Shared {
    fn snapshot(&self) -> EgressStatus {
        let attached = self.attached.load(Ordering::SeqCst);
        EgressStatus {
            enabled: true,
            ready: attached,
            client_attached: attached,
            protocol: PROTOCOL_NAME.to_string(),
            ws: format_bind(self.ws_addr),
            proxy: format_bind(self.proxy_addr),
        }
    }
}

pub struct ServerBinds {
    pub ws: TcpListener,
    pub proxy: TcpListener,
    pub admin: TcpListener,
}

impl ServerBinds {
    pub async fn bind(cfg: &TunnelServerConfig) -> std::io::Result<Self> {
        let ws = TcpListener::bind(cfg.ws_bind).await?;
        let proxy = TcpListener::bind(cfg.proxy_bind).await?;
        let admin = TcpListener::bind(cfg.admin_bind).await?;
        Ok(Self { ws, proxy, admin })
    }
}

pub async fn run(cfg: TunnelServerConfig) -> anyhow::Result<()> {
    let binds = ServerBinds::bind(&cfg).await?;
    run_with_binds(cfg, binds).await
}

pub async fn run_with_binds(cfg: TunnelServerConfig, binds: ServerBinds) -> anyhow::Result<()> {
    let ws_addr = binds.ws.local_addr()?;
    let proxy_addr = binds.proxy.local_addr()?;
    let admin_addr = binds.admin.local_addr()?;
    tracing::info!(
        %ws_addr,
        %proxy_addr,
        %admin_addr,
        protocol = PROTOCOL_NAME,
        "box-egress-tunnel server listening"
    );
    let shared = Arc::new(Shared {
        cfg,
        handshakes: Semaphore::new(MAX_PENDING_HANDSHAKES),
        attached: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        session: Mutex::new(None),
        ws_addr,
        proxy_addr,
    });

    loop {
        tokio::select! {
            accepted = binds.ws.accept() => {
                let Some((stream, peer)) = accept_or_skip("ws", accepted).await else { continue };
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_ws(shared, stream, peer).await {
                        tracing::debug!(error = %err, %peer, "ws session ended");
                    }
                });
            }
            accepted = binds.proxy.accept() => {
                let Some((stream, peer)) = accept_or_skip("proxy", accepted).await else { continue };
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_proxy(shared, stream).await {
                        tracing::debug!(error = %err, %peer, "proxy session ended");
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

async fn handle_admin(shared: Arc<Shared>, mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let (status, body) = if req.starts_with("GET /v1/health") {
        (200, b"{\"status\":\"ok\"}".to_vec())
    } else if req.starts_with("GET /v1/status") {
        (
            200,
            serde_json::to_vec(&shared.snapshot()).unwrap_or_else(|_| b"{}".to_vec()),
        )
    } else {
        (404, b"{\"error\":\"not_found\"}".to_vec())
    };
    let reason = match status {
        200 => "OK",
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

// The handshake callback's error type is tungstenite's `ErrorResponse`, a whole HTTP response: its
// size is the crate's signature, not a choice made here.
#[allow(clippy::result_large_err)]
async fn handle_ws(
    shared: Arc<Shared>,
    stream: TcpStream,
    peer: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let bearer = shared.cfg.bearer.clone();
    let Ok(permit) = shared.handshakes.try_acquire() else {
        tracing::warn!(%peer, "too many pending handshakes; dropping the connection");
        return Ok(());
    };
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

    tracing::info!(%peer, "egress client attached");
    serve_client(shared, ws).await;
    tracing::info!(%peer, "egress client detached");
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

async fn handle_proxy(shared: Arc<Shared>, mut stream: TcpStream) -> anyhow::Result<()> {
    let (head, rest) = match read_http_head(&mut stream).await {
        Ok(v) => v,
        Err(_) => {
            let _ = write_http(&mut stream, 400, "Bad Request", "bad request\n").await;
            return Ok(());
        }
    };
    let Some(req) = parse_proxy_request(&head, &rest) else {
        let _ = write_http(&mut stream, 400, "Bad Request", "bad request\n").await;
        return Ok(());
    };

    if !req.connect {
        // CONNECT only. Absolute-form proxying read one request head, opened one
        // upstream, then relayed every later byte on the client socket to that
        // first upstream -- and Chromium keys plain-http proxy sockets on the
        // proxy, not the origin, so a second site's request went to the first
        // site, cookies and all. Encrypted traffic never used this path.
        let _ = write_http(&mut stream, 405, "Method Not Allowed", "CONNECT only\n").await;
        return Ok(());
    }

    if !shared.cfg.allowlist.allows(&req.host) {
        tracing::info!(host = %req.host, port = req.port, "CONNECT host not in allowlist");
        let _ = write_http(&mut stream, 403, "Forbidden", "host not allowed\n").await;
        return Ok(());
    }

    let prepared = {
        let mut g = shared.session.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some(session) if session.streams.len() < MAX_STREAMS => alloc_id(session).map(|id| {
                let (from_tx, from_rx) = mpsc::channel(256);
                let (opened_tx, opened_rx) = oneshot::channel();
                session.streams.insert(
                    id,
                    StreamSlot {
                        from_client: from_tx,
                        opened: Some(opened_tx),
                    },
                );
                (
                    id,
                    from_rx,
                    opened_rx,
                    session.outgoing.clone(),
                    session.generation,
                )
            }),
            _ => None,
        }
    };
    let Some((id, from_client_rx, opened_rx, outgoing, generation)) = prepared else {
        let _ = write_http(
            &mut stream,
            503,
            "Service Unavailable",
            "box-egress-tunnel: no client attached\n",
        )
        .await;
        return Ok(());
    };

    if outgoing
        .send(WsOut::Control(ControlMsg::open(
            id,
            req.host.clone(),
            req.port,
        )))
        .await
        .is_err()
    {
        drop_stream(&shared, generation, id);
        let _ = write_http(
            &mut stream,
            503,
            "Service Unavailable",
            "box-egress-tunnel: no client attached\n",
        )
        .await;
        return Ok(());
    }

    let opened = tokio::time::timeout(OPEN_TIMEOUT, opened_rx).await;
    match opened {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(message))) => {
            drop_stream(&shared, generation, id);
            let _ = write_http(&mut stream, 502, "Bad Gateway", &format!("{message}\n")).await;
            return Ok(());
        }
        Ok(Err(_)) | Err(_) => {
            let _ = outgoing
                .send(WsOut::Control(ControlMsg::close(id, Some("open-timeout"))))
                .await;
            drop_stream(&shared, generation, id);
            let _ = write_http(
                &mut stream,
                504,
                "Gateway Timeout",
                "client did not open the stream\n",
            )
            .await;
            return Ok(());
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
        drop_stream(&shared, generation, id);
        return Ok(());
    }

    tracing::info!(host = %req.host, port = req.port, id, "relaying CONNECT");
    let _ = stream.set_nodelay(true);
    copy_proxy_stream(stream, from_client_rx, outgoing, id, req.initial).await;
    drop_stream(&shared, generation, id);
    Ok(())
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

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}
