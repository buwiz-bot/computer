//! Local server+client relay tests. No internet.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::TunnelServerConfig;
use crate::destination::DestinationPolicy;
use crate::status::{probe_egress, EgressConfig};
use crate::{client, server, Allowlist, TunnelClientConfig};

const BEARER: &str = "local-smoke-egress-token";

/// These tests relay to a loopback echo server, which the shipped policy
/// refuses on purpose. Opting in here keeps the tests about the relay and
/// keeps the default honest.
fn loopback_destination() -> DestinationPolicy {
    DestinationPolicy::default()
        .with_allow_private(true)
        .with_ports(Some(Vec::new()))
}

struct Kill(tokio::task::JoinHandle<()>);

impl Drop for Kill {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn echo_once(listener: TcpListener) {
    let (mut stream, _) = listener.accept().await.expect("echo accept");
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.expect("echo read");
    stream.write_all(&buf[..n]).await.expect("echo write");
}

async fn start_tunnel() -> (
    Kill,
    std::net::SocketAddr,
    std::net::SocketAddr,
    std::net::SocketAddr,
) {
    let cfg = TunnelServerConfig {
        ws_bind: "127.0.0.1:0".parse().unwrap(),
        proxy_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        bearer: BEARER.to_string(),
        allowlist: Allowlist::any(),
    };
    let binds = server::ServerBinds::bind(&cfg).await.expect("bind");
    let ws = binds.ws.local_addr().unwrap();
    let proxy = binds.proxy.local_addr().unwrap();
    let admin = binds.admin.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = server::run_with_binds(cfg, binds).await;
    });
    (Kill(handle), ws, proxy, admin)
}

async fn wait_admin(admin: std::net::SocketAddr, want_ready: bool) {
    let cfg = EgressConfig {
        enabled: true,
        ws_bind: "127.0.0.1:8790".parse().unwrap(),
        proxy_bind: "127.0.0.1:8791".parse().unwrap(),
        admin_bind: admin,
    };
    for _ in 0..80 {
        let st = probe_egress(&cfg).await;
        if st.enabled && st.ready == want_ready {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "admin never reached ready={want_ready}: {:?}",
        probe_egress(&cfg).await
    );
}

async fn read_http_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        assert!(n > 0, "eof before HTTP head");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos + 4]).into_owned();
            let rest = buf[pos + 4..].to_vec();
            return (head, rest);
        }
        assert!(buf.len() < 8192, "header too large");
    }
}

async fn connect_and_send(
    proxy: std::net::SocketAddr,
    dest: std::net::SocketAddr,
    payload: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let mut stream = TcpStream::connect(proxy)
        .await
        .map_err(|e| format!("dial proxy: {e}"))?;
    let req = format!("CONNECT {dest} HTTP/1.1\r\nHost: {dest}\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write CONNECT: {e}"))?;
    let (head, rest) = read_http_head(&mut stream).await;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Ok((status, rest));
    }
    stream
        .write_all(payload)
        .await
        .map_err(|e| format!("write payload: {e}"))?;
    let mut out = rest;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;
    out.extend_from_slice(&buf[..n]);
    Ok((status, out))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_fail_closed_without_client() {
    let (_kill, _ws, proxy, admin) = start_tunnel().await;
    wait_admin(admin, false).await;
    let dest: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let (status, _) = connect_and_send(proxy, dest, b"hi").await.expect("proxy");
    assert_eq!(status, 503, "enabled but no client must fail closed");
    let st = probe_egress(&EgressConfig {
        enabled: true,
        ws_bind: "127.0.0.1:0".parse().unwrap(),
        proxy_bind: proxy,
        admin_bind: admin,
    })
    .await;
    assert!(st.enabled);
    assert!(!st.ready);
    assert!(!st.client_attached);
}

/// A raw HTTP request to the proxy, returning the status line's code.
async fn raw_status(proxy: std::net::SocketAddr, request: &[u8]) -> u16 {
    let mut s = TcpStream::connect(proxy).await.expect("proxy connect");
    s.write_all(request).await.expect("write");
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).await.expect("read");
    let head = String::from_utf8_lossy(&buf[..n]);
    head.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_http_is_refused_even_with_a_client_attached() {
    let (_kill, ws, proxy, admin) = start_tunnel().await;
    wait_admin(admin, false).await;
    let client_cfg = TunnelClientConfig {
        url: format!("ws://{ws}"),
        bearer: BEARER.to_string(),
        allowlist: Allowlist::any(),
        destination: loopback_destination(),
        reconnect: false,
    };
    let _client = Kill(tokio::spawn(async move {
        let _ = client::run(client_cfg).await;
    }));
    wait_admin(admin, true).await;
    let status = raw_status(
        proxy,
        b"GET http://example.com/private HTTP/1.1\r\nHost: example.com\r\n\r\n",
    )
    .await;
    assert_eq!(status, 405, "absolute-form HTTP must not be relayed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_roundtrip_via_client() {
    let echo_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_l.local_addr().unwrap();
    let echo_task = tokio::spawn(echo_once(echo_l));

    let (_kill, ws, proxy, admin) = start_tunnel().await;
    wait_admin(admin, false).await;

    let client_cfg = TunnelClientConfig {
        url: format!("ws://{ws}"),
        bearer: BEARER.to_string(),
        allowlist: Allowlist::any(),
        destination: loopback_destination(),
        reconnect: false,
    };
    let client = Kill(tokio::spawn(async move {
        let _ = client::run(client_cfg).await;
    }));
    wait_admin(admin, true).await;

    let payload = b"hello-via-relay";
    let (status, got) = connect_and_send(proxy, echo_addr, payload)
        .await
        .expect("CONNECT");
    assert_eq!(status, 200);
    assert_eq!(
        got, payload,
        "bytes must round-trip through the laptop relay"
    );

    echo_task.await.unwrap();
    drop(client);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_rejects_wrong_bearer() {
    let (_kill, ws, _proxy, _admin) = start_tunnel().await;
    let client_cfg = TunnelClientConfig {
        url: format!("ws://{ws}"),
        bearer: "totally-wrong-token".to_string(),
        allowlist: Allowlist::any(),
        destination: loopback_destination(),
        reconnect: false,
    };
    let err = client::run_once(&client_cfg)
        .await
        .expect_err("wrong bearer must fail");
    let text = format!("{err:#}").to_ascii_lowercase();
    assert!(
        text.contains("401") || text.contains("unauthorized") || text.contains("http"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowlist_rejects_host() {
    let echo_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_l.local_addr().unwrap();

    let cfg = TunnelServerConfig {
        ws_bind: "127.0.0.1:0".parse().unwrap(),
        proxy_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        bearer: BEARER.to_string(),
        allowlist: Allowlist::parse("only.example"),
    };
    let binds = server::ServerBinds::bind(&cfg).await.unwrap();
    let ws = binds.ws.local_addr().unwrap();
    let proxy = binds.proxy.local_addr().unwrap();
    let admin = binds.admin.local_addr().unwrap();
    let _kill = Kill(tokio::spawn(async move {
        let _ = server::run_with_binds(cfg, binds).await;
    }));

    let client_cfg = TunnelClientConfig {
        url: format!("ws://{ws}"),
        bearer: BEARER.to_string(),
        allowlist: Allowlist::any(),
        destination: loopback_destination(),
        reconnect: false,
    };
    let _client = Kill(tokio::spawn(async move {
        let _ = client::run(client_cfg).await;
    }));
    wait_admin(admin, true).await;

    let (status, _) = connect_and_send(proxy, echo_addr, b"x").await.unwrap();
    assert_eq!(status, 403);
}
