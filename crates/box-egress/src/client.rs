//! The operator-side client: attach to the guest's tunnel and dial outbound TCP for it.
//!
//! It judges every stream twice before dialling: by the computer's rules on the name the guest
//! asked for (the guest's proxy judged it too; this end is the authority for traffic leaving the
//! operator's network), then by the address the name resolved to ([`crate::destination`]). It
//! logs no host names: browsing hosts are private. Its [`ClientState`] says whether it is
//! attached, and what last went wrong, in words that name no site.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{header, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::allowlist::Allowlist;
use crate::destination::DestinationPolicy;
use crate::dial::resolve_and_dial;
use crate::protocol::{
    decode_data, encode_data, ControlMsg, CHUNK, KEEPALIVE_INTERVAL, KEEPALIVE_MISSES, MAX_FRAME,
    MAX_MESSAGE, PROTOCOL_NAME,
};
use crate::rules::{RulesCell, Verdict};

/// Longest the WebSocket handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first wait before a reconnect, and the longest.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Chunks queued toward one destination before that stream counts as not keeping up.
const STREAM_QUEUE: usize = 32;
/// Streams this end dials at once, for one session: the guest's own cap (256), so a box can't
/// make this Mac open more than it could ever relay. One past it is refused.
pub const MAX_OPEN: usize = 256;

/// What the client needs. `rules` is read on every stream, so a change to it applies to the next
/// stream without a reconnect.
#[derive(Clone, Debug)]
pub struct TunnelClientConfig {
    pub url: String,
    pub bearer: String,
    /// The operator's static name filter (`BOX_EGRESS_RELAY_HOSTS`). Empty is no extra filter.
    pub allowlist: Allowlist,
    /// Resolved-address and port policy applied immediately before each dial.
    pub destination: DestinationPolicy,
    /// The computer's rules. Empty refuses every stream.
    pub rules: RulesCell,
    pub reconnect: bool,
    pub state: ClientState,
}

/// Why the client stopped or a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The box refused the bearer (HTTP 401). Retrying the same bearer can't help.
    Unauthorized,
    /// The URL isn't a WebSocket URL.
    BadUrl,
    /// Couldn't reach the box, or the handshake failed.
    Connect(String),
    /// Attached, then the session ended badly.
    Session(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("the box refused the tunnel's bearer (401)"),
            Self::BadUrl => f.write_str("the tunnel URL isn't a ws:// or wss:// URL"),
            Self::Connect(why) => write!(f, "couldn't attach: {why}"),
            Self::Session(why) => write!(f, "the session ended: {why}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// What the client is doing, for whoever runs it. No field names a site.
#[derive(Clone, Debug, Default)]
pub struct ClientState(Arc<StateInner>);

#[derive(Debug, Default)]
struct StateInner {
    attached: AtomicBool,
    attaches: AtomicU64,
    opened: AtomicU64,
    refused: AtomicU64,
    failed: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// A copy of [`ClientState`] at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientSnapshot {
    pub attached: bool,
    /// Successful attaches since this client started.
    pub attaches: u64,
    /// Streams dialled.
    pub opened: u64,
    /// Streams refused by the rules or the address policy.
    pub refused: u64,
    /// Streams whose dial failed.
    pub failed: u64,
    pub last_error: Option<String>,
}

impl ClientState {
    pub fn snapshot(&self) -> ClientSnapshot {
        ClientSnapshot {
            attached: self.0.attached.load(Ordering::SeqCst),
            attaches: self.0.attaches.load(Ordering::SeqCst),
            opened: self.0.opened.load(Ordering::SeqCst),
            refused: self.0.refused.load(Ordering::SeqCst),
            failed: self.0.failed.load(Ordering::SeqCst),
            last_error: self
                .0
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    fn set_attached(&self, on: bool) {
        self.0.attached.store(on, Ordering::SeqCst);
        if on {
            self.0.attaches.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn set_error(&self, err: &ClientError) {
        *self.0.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
    }
}

enum WsOut {
    Control(ControlMsg),
    Data { id: u32, payload: Vec<u8> },
    Ping,
}

/// The session's streams, by id. Each holds a generation, so a stream's task removes only its own
/// entry: an id reused by a newer stream is never dropped by an older one's ending.
type Slots = HashMap<u32, (u64, Option<mpsc::Sender<Vec<u8>>>)>;

struct Streams {
    map: Mutex<Slots>,
    next: AtomicU64,
}

impl Streams {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes `id` for a new stream: its generation, or `None` while another stream holds it.
    fn reserve(&self, id: u32) -> Option<u64> {
        let mut map = self.lock();
        if map.contains_key(&id) {
            return None;
        }
        let gen = self.next.fetch_add(1, Ordering::SeqCst);
        map.insert(id, (gen, None));
        Some(gen)
    }

    /// The reserved stream's queue toward its destination, once it's dialled.
    fn open(&self, id: u32, gen: u64, tx: mpsc::Sender<Vec<u8>>) {
        if let Some(slot) = self.lock().get_mut(&id).filter(|(g, _)| *g == gen) {
            slot.1 = Some(tx);
        }
    }

    fn get(&self, id: u32) -> Option<mpsc::Sender<Vec<u8>>> {
        self.lock().get(&id).and_then(|(_, tx)| tx.clone())
    }

    /// The box closed `id`, whichever stream holds it now.
    fn remove(&self, id: u32) {
        self.lock().remove(&id);
    }

    /// The stream of generation `gen` ended: its entry goes, and only its own.
    fn release(&self, id: u32, gen: u64) {
        let mut map = self.lock();
        if map.get(&id).is_some_and(|(g, _)| *g == gen) {
            map.remove(&id);
        }
    }
}

/// Ends every stream of a session when the session goes, however it goes (a return, an error, or
/// the task running it being aborted): each stream's task sees its queue close and stops, so no
/// outbound socket outlives the session.
struct SessionGuard {
    streams: Arc<Streams>,
    state: ClientState,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.streams.lock().clear();
        self.state.set_attached(false);
    }
}

/// Attach, and with `reconnect`, attach again after each drop, waiting [`BACKOFF_FIRST`] doubling
/// to [`BACKOFF_MAX`], reset by each successful attach. A refused bearer ends it.
pub async fn run(cfg: TunnelClientConfig) -> Result<(), ClientError> {
    if !cfg.reconnect {
        return run_once(&cfg).await;
    }
    let mut wait = BACKOFF_FIRST;
    loop {
        let attaches = cfg.state.snapshot().attaches;
        match run_once(&cfg).await {
            Ok(()) => tracing::warn!("egress tunnel detached; attaching again"),
            Err(ClientError::Unauthorized) => {
                cfg.state.set_error(&ClientError::Unauthorized);
                return Err(ClientError::Unauthorized);
            }
            Err(ClientError::BadUrl) => {
                cfg.state.set_error(&ClientError::BadUrl);
                return Err(ClientError::BadUrl);
            }
            Err(err) => {
                tracing::warn!(error = %err, "egress tunnel client error; attaching again");
                cfg.state.set_error(&err);
            }
        }
        if cfg.state.snapshot().attaches > attaches {
            wait = BACKOFF_FIRST;
        }
        tokio::time::sleep(wait).await;
        wait = next_backoff(wait);
    }
}

fn next_backoff(wait: Duration) -> Duration {
    (wait * 2).min(BACKOFF_MAX)
}

/// One attach, for as long as it lasts.
pub async fn run_once(cfg: &TunnelClientConfig) -> Result<(), ClientError> {
    let mut request = cfg
        .url
        .as_str()
        .into_client_request()
        .map_err(|_| ClientError::BadUrl)?;
    let bearer = format!("Bearer {}", cfg.bearer);
    let mut value = HeaderValue::from_str(&bearer)
        .map_err(|_| ClientError::Connect("the bearer isn't a header value".into()))?;
    value.set_sensitive(true);
    request.headers_mut().insert(header::AUTHORIZATION, value);
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(PROTOCOL_NAME),
    );

    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_FRAME));
    let connect = connect_async_with_config(request, Some(ws_config), false);
    let (ws, _resp) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connect).await {
        Err(_) => return Err(ClientError::Connect("the handshake timed out".into())),
        Ok(Err(WsError::Http(resp))) if resp.status() == StatusCode::UNAUTHORIZED => {
            return Err(ClientError::Unauthorized)
        }
        Ok(Err(WsError::Http(resp))) => {
            return Err(ClientError::Connect(format!("HTTP {}", resp.status())))
        }
        Ok(Err(WsError::Url(_))) => return Err(ClientError::BadUrl),
        Ok(Err(err)) => return Err(ClientError::Connect(err.to_string())),
        Ok(Ok(pair)) => pair,
    };
    tracing::info!(protocol = PROTOCOL_NAME, "egress tunnel attached");
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(256);
    let streams = Arc::new(Streams::new());
    let opens = Arc::new(tokio::sync::Semaphore::new(MAX_OPEN));
    cfg.state.set_attached(true);
    let _guard = SessionGuard {
        streams: streams.clone(),
        state: cfg.state.clone(),
    };

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
    let _writer = AbortOnDrop(writer);

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await;
    let mut silent_ticks: u8 = 0;
    loop {
        let frame = tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(frame)) => frame,
                Some(Err(err)) => return Err(ClientError::Session(err.to_string())),
                None => return Ok(()),
            },
            _ = keepalive.tick() => {
                if silent_ticks >= KEEPALIVE_MISSES {
                    return Err(ClientError::Session(format!(
                        "the box was silent for {}s",
                        KEEPALIVE_INTERVAL.as_secs() * u64::from(KEEPALIVE_MISSES)
                    )));
                }
                silent_ticks += 1;
                if out_tx.send(WsOut::Ping).await.is_err() {
                    return Ok(());
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
                        let Some(gen) = streams.reserve(id) else {
                            cfg.state.0.refused.fetch_add(1, Ordering::SeqCst);
                            send_soon(
                                &out_tx,
                                WsOut::Control(ControlMsg::error(id, "stream id in use")),
                            );
                            continue;
                        };
                        let Ok(slot) = opens.clone().try_acquire_owned() else {
                            streams.release(id, gen);
                            cfg.state.0.refused.fetch_add(1, Ordering::SeqCst);
                            send_soon(
                                &out_tx,
                                WsOut::Control(ControlMsg::error(id, "too many streams")),
                            );
                            continue;
                        };
                        let job = Open {
                            id,
                            gen,
                            host,
                            port,
                            allow: cfg.allowlist.clone(),
                            rules: cfg.rules.clone(),
                            policy: cfg.destination.clone(),
                            outgoing: out_tx.clone(),
                            streams: streams.clone(),
                            state: cfg.state.clone(),
                        };
                        tokio::spawn(async move {
                            job.run().await;
                            drop(slot);
                        });
                    }
                    ControlMsg::Close { id, .. } | ControlMsg::Error { id, .. } => {
                        streams.remove(id);
                    }
                    ControlMsg::Hello { .. } | ControlMsg::Opened { .. } => {}
                }
            }
            Message::Binary(bin) => {
                let Some((id, payload)) = decode_data(&bin) else {
                    continue;
                };
                let Some(tx) = streams.get(id) else {
                    continue;
                };
                match tx.try_send(payload.to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Awaiting here parked the whole reader, every other stream and all
                        // control frames, behind one destination that stopped reading. Close that
                        // one stream instead, as the guest does.
                        tracing::warn!(id, "a destination isn't keeping up; closing its stream");
                        streams.remove(id);
                        send_soon(
                            &out_tx,
                            WsOut::Control(ControlMsg::close(id, Some("slow consumer"))),
                        );
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => streams.remove(id),
                }
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Ok(()),
        }
    }
}

/// `msg` toward the box without the reader ever waiting on it: at once when the writer's queue has
/// room, else from a task of its own. The reader parked on a full queue was every stream's and
/// every control frame's wait behind one.
fn send_soon(out: &mpsc::Sender<WsOut>, msg: WsOut) {
    if let Err(mpsc::error::TrySendError::Full(msg)) = out.try_send(msg) {
        let out = out.clone();
        tokio::spawn(async move {
            let _ = out.send(msg).await;
        });
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Open {
    id: u32,
    gen: u64,
    host: String,
    port: u16,
    allow: Allowlist,
    rules: RulesCell,
    policy: DestinationPolicy,
    outgoing: mpsc::Sender<WsOut>,
    streams: Arc<Streams>,
    state: ClientState,
}

impl Open {
    async fn refuse(&self, why: &str) {
        self.streams.release(self.id, self.gen);
        self.state.0.refused.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .outgoing
            .send(WsOut::Control(ControlMsg::error(self.id, why)))
            .await;
    }

    async fn run(self) {
        let id = self.id;
        // The rules first, on the name, before any DNS: the guest judged it too, and this end is
        // the authority for what leaves the operator's network.
        let verdict = match self.rules.get() {
            Some(rules) => rules.judge(&self.host),
            None => {
                tracing::info!(id, reason = "no-rules", "stream refused");
                return self.refuse("no rules are set for this computer").await;
            }
        };
        if verdict != Verdict::Allowed {
            tracing::info!(id, reason = verdict.reason(), "stream refused");
            let why = match verdict {
                Verdict::Blocked => "blocked by this computer's rules",
                Verdict::NotListed => "not on this computer's allow list",
                Verdict::Address => "an address, and this computer blocks sites by name",
                Verdict::BadHost | Verdict::Allowed => "not a valid host",
            };
            return self.refuse(why).await;
        }
        if !self.allow.allows(&self.host) {
            tracing::info!(id, reason = "relay-hosts", "stream refused");
            return self.refuse("host not allowed").await;
        }
        match resolve_and_dial(&self.host, self.port, &self.policy).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let (from_tx, from_rx) = mpsc::channel(STREAM_QUEUE);
                self.streams.open(id, self.gen, from_tx);
                if self
                    .outgoing
                    .send(WsOut::Control(ControlMsg::opened(id)))
                    .await
                    .is_err()
                {
                    self.streams.release(id, self.gen);
                    return;
                }
                self.state.0.opened.fetch_add(1, Ordering::SeqCst);
                tracing::debug!(id, "outbound stream open");
                copy_outbound(stream, from_rx, &self.outgoing, id).await;
                self.streams.release(id, self.gen);
            }
            Err(err) => {
                // Logged at warn without the host: on the operator's own machine a refused address
                // is the signal that something in the guest tried to reach their network.
                tracing::warn!(id, reason = err.reason(), "outbound stream not opened");
                if matches!(err, crate::dial::DialError::Refused(_)) {
                    self.refuse(&err.to_string()).await;
                } else {
                    self.streams.release(id, self.gen);
                    self.state.0.failed.fetch_add(1, Ordering::SeqCst);
                    let _ = self
                        .outgoing
                        .send(WsOut::Control(ControlMsg::error(id, err.to_string())))
                        .await;
                }
            }
        }
    }
}

async fn copy_outbound(
    mut stream: TcpStream,
    mut from_server: mpsc::Receiver<Vec<u8>>,
    outgoing: &mpsc::Sender<WsOut>,
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
mod tests {
    use super::*;

    #[test]
    fn a_stream_ending_never_takes_a_newer_streams_place() {
        let streams = Streams::new();
        let old = streams.reserve(7).unwrap();
        assert_eq!(
            streams.reserve(7),
            None,
            "an id in use can't be opened again"
        );
        // the box closes 7 and opens it again: the old stream's end leaves the new one alone
        streams.remove(7);
        let new = streams.reserve(7).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        streams.open(7, new, tx);
        streams.release(7, old);
        assert!(streams.get(7).is_some(), "the newer stream is still there");
        streams.release(7, new);
        assert!(streams.get(7).is_none());
    }

    #[tokio::test]
    async fn the_reader_never_waits_on_a_stalled_writer() {
        let (tx, mut rx) = mpsc::channel::<WsOut>(1);
        tx.send(WsOut::Ping).await.unwrap();
        // the writer's queue is full and nobody drains it: this returns at once
        tokio::time::timeout(Duration::from_millis(100), async {
            send_soon(
                &tx,
                WsOut::Control(ControlMsg::close(7, Some("slow consumer"))),
            );
        })
        .await
        .expect("send_soon waited on a full queue");
        // and the close still goes once the writer moves
        assert!(matches!(rx.recv().await, Some(WsOut::Ping)));
        assert!(matches!(
            rx.recv().await,
            Some(WsOut::Control(ControlMsg::Close { id: 7, .. }))
        ));
    }

    // the handshake callback's error is tungstenite's whole HTTP response, its type not a choice
    #[allow(clippy::result_large_err)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn past_its_cap_an_open_is_refused_and_the_rest_are_dialled() {
        // a destination that takes every connection and keeps it open
        let hold = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = hold.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut kept = Vec::new();
            while let Ok((s, _)) = hold.accept().await {
                kept.push(s);
            }
        });
        // a box that asks for one stream past the cap
        let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", ws.local_addr().unwrap());
        let (said_tx, mut said) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let (s, _) = ws.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(s, |_: &tokio_tungstenite::tungstenite::handshake::server::Request, mut r: tokio_tungstenite::tungstenite::handshake::server::Response| {
                r.headers_mut().insert("sec-websocket-protocol", PROTOCOL_NAME.parse().unwrap());
                Ok(r)
            })
            .await
            .unwrap();
            for id in 1..=(MAX_OPEN as u32 + 1) {
                let open =
                    serde_json::to_string(&ControlMsg::open(id, "127.0.0.1".into(), port)).unwrap();
                ws.send(Message::Text(open.into())).await.unwrap();
            }
            while let Some(Ok(m)) = ws.next().await {
                if let Message::Text(t) = m {
                    let _ = said_tx.send(t.to_string());
                }
            }
        });
        let state = ClientState::default();
        let cfg = TunnelClientConfig {
            url,
            bearer: "x".repeat(43),
            allowlist: Allowlist::any(),
            destination: DestinationPolicy::default()
                .with_allow_private(true)
                .with_ports(Some(Vec::new())),
            rules: RulesCell::new(Some(
                crate::rules::Rules::new(
                    1,
                    crate::rules::Exit::Tunnel,
                    crate::rules::Mode::All,
                    &[],
                    &[],
                )
                .unwrap(),
            )),
            reconnect: false,
            state: state.clone(),
        };
        let _client = AbortOnDrop(tokio::spawn(async move {
            let _ = run(cfg).await;
        }));
        let (mut opened, mut refused) = (0, 0);
        while opened + refused < MAX_OPEN + 1 {
            let t = tokio::time::timeout(Duration::from_secs(10), said.recv())
                .await
                .unwrap()
                .unwrap();
            match serde_json::from_str::<ControlMsg>(&t).unwrap() {
                ControlMsg::Opened { .. } => opened += 1,
                ControlMsg::Error { message, .. } => {
                    assert_eq!(message, "too many streams");
                    refused += 1;
                }
                _ => {}
            }
        }
        assert_eq!((opened, refused), (MAX_OPEN, 1));
    }

    #[test]
    fn backoff_doubles_to_its_cap() {
        let mut wait = BACKOFF_FIRST;
        let mut seen = vec![wait];
        for _ in 0..8 {
            wait = next_backoff(wait);
            seen.push(wait);
        }
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[4], Duration::from_secs(16));
        assert_eq!(*seen.last().unwrap(), BACKOFF_MAX);
    }

    fn config(addr: std::net::SocketAddr, state: &ClientState) -> TunnelClientConfig {
        TunnelClientConfig {
            url: format!("ws://{addr}"),
            bearer: "x".repeat(43),
            allowlist: Allowlist::any(),
            destination: DestinationPolicy::default(),
            rules: RulesCell::default(),
            reconnect: true,
            state: state.clone(),
        }
    }

    #[tokio::test]
    async fn a_refused_bearer_is_told_apart_by_its_status_and_ends_the_client() {
        // A loopback listener that answers every upgrade with 401, as the box does.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\nunauthorized\n",
                    )
                    .await;
            }
        });
        let state = ClientState::default();
        let got = tokio::time::timeout(Duration::from_secs(5), run(config(addr, &state)))
            .await
            .expect("a 401 must end the client, not retry it");
        assert_eq!(got, Err(ClientError::Unauthorized));
        let snap = state.snapshot();
        assert!(!snap.attached);
        assert!(snap.last_error.unwrap().contains("401"));
    }

    #[tokio::test]
    async fn a_page_that_says_unauthorized_in_a_500_is_not_a_401() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(b"HTTP/1.1 500 Oops\r\nContent-Length: 16\r\n\r\n401 unauthorized")
                    .await;
            }
        });
        let state = ClientState::default();
        let mut cfg = config(addr, &state);
        cfg.reconnect = false;
        let got = run(cfg).await;
        assert!(matches!(got, Err(ClientError::Connect(_))), "{got:?}");
    }

    #[tokio::test]
    async fn nothing_listening_is_a_connect_error_that_is_retried() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let state = ClientState::default();
        let run = tokio::spawn(run(config(addr, &state)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !run.is_finished(),
            "a client that can't reach the box keeps trying"
        );
        run.abort();
        let snap = state.snapshot();
        assert!(snap.last_error.unwrap().starts_with("couldn't attach"));
    }
}
