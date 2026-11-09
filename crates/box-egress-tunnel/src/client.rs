//! Laptop-side client: attach to the guest WS and dial outbound TCP.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{header, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::config::TunnelClientConfig;
use crate::protocol::{decode_data, encode_data, ControlMsg, PROTOCOL_NAME};

const CHUNK: usize = 32 * 1024;

/// Longest dialling a destination may take, over all of its vetted addresses. Below the server's
/// `OPEN_TIMEOUT` (15 s), so the guest hears why a stream failed rather than giving up on it.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// The least time one address gets inside [`DIAL_TIMEOUT`], so a black-holed first address
/// leaves time for the next.
const ATTEMPT_FLOOR: Duration = Duration::from_secs(3);

enum WsOut {
    Control(ControlMsg),
    Data { id: u32, payload: Vec<u8> },
    Ping,
}

/// Mirror of the server's keepalive, so a laptop whose link died notices and
/// its `--reconnect` loop can re-attach instead of sitting on a dead socket.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_MISSES: u8 = 2;

struct Streams {
    map: Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>,
}

impl Streams {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, id: u32, tx: mpsc::Sender<Vec<u8>>) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
    }

    fn get(&self, id: u32) -> Option<mpsc::Sender<Vec<u8>>> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    fn remove(&self, id: u32) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

pub async fn run(cfg: TunnelClientConfig) -> anyhow::Result<()> {
    if cfg.reconnect {
        loop {
            match run_once(&cfg).await {
                Ok(()) => {
                    tracing::warn!("tunnel disconnected; reconnecting in 2s");
                }
                Err(err) => {
                    if is_unauthorized(&err) {
                        return Err(err);
                    }
                    tracing::error!(error = %err, "tunnel client error; retrying in 2s");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    } else {
        run_once(&cfg).await
    }
}

fn is_unauthorized(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("401") || text.contains("unauthorized")
}

pub async fn run_once(cfg: &TunnelClientConfig) -> anyhow::Result<()> {
    let mut request = cfg
        .url
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid websocket URL {}", cfg.url))?;
    let bearer = format!("Bearer {}", cfg.bearer);
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&bearer).map_err(|err| anyhow!("bearer header: {err}"))?,
    );
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(PROTOCOL_NAME),
    );

    tracing::info!(url = %cfg.url, protocol = PROTOCOL_NAME, "connecting egress client");
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(256 << 10));
    let (ws, _resp) = connect_async_with_config(request, Some(ws_config), false)
        .await
        .with_context(|| format!("connect {}", cfg.url))?;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(256);
    let streams = Arc::new(Streams::new());
    let policy = cfg.destination.clone();

    let _ = out_tx
        .send(WsOut::Control(ControlMsg::hello("client")))
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
    keepalive.tick().await;
    let mut silent_ticks: u8 = 0;
    // Every exit goes through the cleanup after the loop, so the writer task is
    // always aborted; a `?` here used to return early and leak it.
    let mut outcome: anyhow::Result<()> = Ok(());
    loop {
        let frame = tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(frame)) => frame,
                Some(Err(err)) => {
                    outcome = Err(anyhow::Error::new(err).context("websocket read"));
                    break;
                }
                None => break,
            },
            _ = keepalive.tick() => {
                if silent_ticks >= KEEPALIVE_MISSES {
                    outcome = Err(anyhow!(
                        "egress server silent for {}s",
                        KEEPALIVE_INTERVAL.as_secs() * u64::from(KEEPALIVE_MISSES)
                    ));
                    break;
                }
                silent_ticks += 1;
                if out_tx.send(WsOut::Ping).await.is_err() {
                    break;
                }
                continue;
            }
        };
        silent_ticks = 0;
        match frame {
            Message::Text(text) => {
                let Ok(msg) = serde_json::from_str::<ControlMsg>(&text) else {
                    continue;
                };
                if !msg.wire_ok() {
                    continue;
                }
                match msg {
                    ControlMsg::Open { id, host, port, .. } => {
                        let allow = cfg.allowlist.clone();
                        let policy = policy.clone();
                        let outgoing = out_tx.clone();
                        let streams = streams.clone();
                        tokio::spawn(async move {
                            handle_open(id, host, port, allow, policy, outgoing, streams).await;
                        });
                    }
                    ControlMsg::Close { id, .. } | ControlMsg::Error { id, .. } => {
                        streams.remove(id);
                    }
                    ControlMsg::Hello { .. } | ControlMsg::Opened { .. } => {}
                }
            }
            Message::Binary(bin) => {
                if let Some((id, payload)) = decode_data(&bin) {
                    if let Some(tx) = streams.get(id) {
                        let _ = tx.send(payload.to_vec()).await;
                    }
                }
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => break,
        }
    }

    writer.abort();
    outcome
}

async fn handle_open(
    id: u32,
    host: String,
    port: u16,
    allow: crate::Allowlist,
    policy: crate::destination::DestinationPolicy,
    outgoing: mpsc::Sender<WsOut>,
    streams: Arc<Streams>,
) {
    if !allow.allows(&host) {
        let _ = outgoing
            .send(WsOut::Control(ControlMsg::error(id, "host not allowed")))
            .await;
        return;
    }
    let dest = format!("{host}:{port}");

    // Resolve, then judge what it resolved TO. The host string came from the
    // guest browser, so it proves nothing: `localhost`, a name with an A record
    // pointing at 192.168.1.1, and `169.254.169.254` are all just strings.
    let resolved: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(addrs) => addrs.collect(),
            Err(err) => {
                tracing::warn!(%dest, id, error = %err, "outbound DNS failed");
                let _ = outgoing
                    .send(WsOut::Control(ControlMsg::error(id, "resolve failed")))
                    .await;
                return;
            }
        };
    let refused_reason = resolved
        .first()
        .and_then(|addr| policy.refuse_reason(addr))
        .unwrap_or("no address");
    let vetted = policy.vet(resolved);
    if vetted.is_empty() {
        // Logged at warn: on the operator's own machine this is the signal
        // that something in the guest tried to reach their network.
        tracing::warn!(%dest, id, reason = refused_reason, "refused: not a public destination");
        let _ = outgoing
            .send(WsOut::Control(ControlMsg::error(id, "destination refused")))
            .await;
        return;
    }
    // Dial the addresses that were vetted, in order, and NOT the hostname
    // again -- resolving a second time is what lets a DNS rebind swap in the
    // address just refused. Trying each keeps the multi-record fallback that
    // `connect((host, port))` used to give for free.
    match dial(vetted, DIAL_TIMEOUT, ATTEMPT_FLOOR).await {
        Ok(stream) => {
            let _ = stream.set_nodelay(true);
            let (from_tx, from_rx) = mpsc::channel(32);
            streams.insert(id, from_tx);
            if outgoing
                .send(WsOut::Control(ControlMsg::opened(id)))
                .await
                .is_err()
            {
                streams.remove(id);
                return;
            }
            tracing::info!(%dest, id, "outbound TCP open");
            copy_outbound(stream, from_rx, outgoing, id).await;
            streams.remove(id);
        }
        Err(err) => {
            tracing::warn!(%dest, id, error = %err, "outbound TCP failed");
            let _ = outgoing
                .send(WsOut::Control(ControlMsg::error(id, err.to_string())))
                .await;
        }
    }
}

/// Connect to the first of `addrs` that answers, all within `budget`. Each address gets its share
/// of `budget`, at least `floor`, and never more than what is left.
async fn dial(
    addrs: Vec<std::net::SocketAddr>,
    budget: Duration,
    floor: Duration,
) -> std::io::Result<TcpStream> {
    let timed_out = |after: Duration| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("connect timed out after {}ms", after.as_millis()),
        )
    };
    let deadline = tokio::time::Instant::now() + budget;
    let share = (budget / addrs.len().max(1) as u32).max(floor);
    let mut last = std::io::Error::other("no vetted address");
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timed_out(budget));
        }
        let cap = share.min(remaining);
        match tokio::time::timeout(cap, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(err)) => last = err,
            Err(_) => last = timed_out(cap),
        }
    }
    Err(last)
}

async fn copy_outbound(
    mut stream: TcpStream,
    mut from_server: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<WsOut>,
    id: u32,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            n = stream.read(&mut buf) => {
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
            msg = from_server.recv() => {
                match msg {
                    Some(data) => {
                        if stream.write_all(&data).await.is_err() {
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

#[cfg(test)]
mod dial_tests {
    use super::*;

    #[tokio::test]
    async fn a_dead_address_falls_through_to_the_next() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = closed.local_addr().unwrap();
        drop(closed);
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_addr = open.local_addr().unwrap();
        let stream = dial(vec![closed_addr, open_addr], DIAL_TIMEOUT, ATTEMPT_FLOOR)
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open_addr);
    }

    #[tokio::test]
    async fn a_destination_that_never_answers_stops_at_the_budget() {
        // TEST-NET-1: nothing routes there, so the connect either hangs or fails at once.
        let started = std::time::Instant::now();
        let dead = "192.0.2.1:9".parse().unwrap();
        let err = dial(
            vec![dead],
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
    }

    #[tokio::test]
    async fn a_black_holed_first_address_leaves_time_for_the_next() {
        // TEST-NET-1 never answers; the live second address must still be reached in the budget.
        let dead = "192.0.2.1:9".parse().unwrap();
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_addr = open.local_addr().unwrap();
        let started = std::time::Instant::now();
        let stream = dial(
            vec![dead, open_addr],
            Duration::from_millis(1000),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open_addr);
        assert!(started.elapsed() < Duration::from_millis(1000));
    }

    #[test]
    fn the_whole_dial_ends_before_the_guest_gives_up() {
        assert!(DIAL_TIMEOUT < crate::server::OPEN_TIMEOUT);
        assert!(ATTEMPT_FLOOR < DIAL_TIMEOUT);
    }
}
