//! Local server+client relay tests. No internet: every destination is a loopback listener.

use std::time::Duration;

use box_egress::client::ClientState;
use box_egress::rules::{Exit, Mode, Rules, RulesCell};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{TunnelServerConfig, WsSide};
use crate::destination::DestinationPolicy;
use crate::status::{probe_egress, EgressConfig, EgressStatus};
use crate::{client, server, Allowlist, TunnelClientConfig};

const BEARER: &str = "local-smoke-egress-token-that-is-long-enough";

/// These tests relay to a loopback echo server, which the shipped policy
/// refuses on purpose. Opting in here keeps the tests about the relay and
/// keeps the default honest.
fn loopback_destination() -> DestinationPolicy {
    DestinationPolicy::default()
        .with_allow_private(true)
        .with_ports(Some(Vec::new()))
}

fn rules(exit: Exit, mode: Mode, allow: &[&str], block: &[&str]) -> Rules {
    let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
    let block: Vec<String> = block.iter().map(|s| s.to_string()).collect();
    Rules::new(1, exit, mode, &allow, &block).unwrap()
}

fn every_site(exit: Exit) -> Rules {
    rules(exit, Mode::All, &[], &[])
}

struct Kill(tokio::task::JoinHandle<()>);

impl Drop for Kill {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn echo(listener: TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let Ok(n) = stream.read(&mut buf).await else {
                return;
            };
            let _ = stream.write_all(&buf[..n]).await;
        });
    }
}

async fn echo_server() -> (Kill, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    (Kill(tokio::spawn(echo(l))), addr)
}

struct Tunnel {
    _kill: Kill,
    ws: Option<std::net::SocketAddr>,
    proxy: std::net::SocketAddr,
    admin: std::net::SocketAddr,
    rules: RulesCell,
}

async fn start(
    with_tunnel: bool,
    rules: Option<Rules>,
    destination: DestinationPolicy,
    allowlist: Allowlist,
    rules_path: Option<std::path::PathBuf>,
) -> Tunnel {
    let cell = RulesCell::new(rules);
    let cfg = TunnelServerConfig {
        tunnel: with_tunnel.then(|| WsSide {
            bind: "127.0.0.1:0".parse().unwrap(),
            bearer: BEARER.to_string(),
        }),
        proxy_bind: "127.0.0.1:0".parse().unwrap(),
        admin_bind: "127.0.0.1:0".parse().unwrap(),
        allowlist,
        destination,
        rules_path,
        rules: cell.clone(),
    };
    let binds = server::ServerBinds::bind(&cfg).await.expect("bind");
    let ws = binds.ws.as_ref().map(|l| l.local_addr().unwrap());
    let proxy = binds.proxy.local_addr().unwrap();
    let admin = binds.admin.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = server::run_with_binds(cfg, binds).await;
    });
    Tunnel {
        _kill: Kill(handle),
        ws,
        proxy,
        admin,
        rules: cell,
    }
}

async fn start_tunnel(rules: Option<Rules>) -> Tunnel {
    start(
        true,
        rules,
        DestinationPolicy::default(),
        Allowlist::any(),
        None,
    )
    .await
}

fn client_cfg(ws: std::net::SocketAddr, rules: Option<Rules>) -> TunnelClientConfig {
    TunnelClientConfig {
        url: format!("ws://{ws}"),
        bearer: BEARER.to_string(),
        allowlist: Allowlist::any(),
        destination: loopback_destination(),
        rules: RulesCell::new(rules),
        reconnect: false,
        state: ClientState::default(),
    }
}

fn attach(ws: std::net::SocketAddr, rules: Option<Rules>) -> Kill {
    let cfg = client_cfg(ws, rules);
    Kill(tokio::spawn(async move {
        let _ = client::run(cfg).await;
    }))
}

async fn status(t: &Tunnel) -> EgressStatus {
    probe_egress(&EgressConfig {
        enabled: t.ws.is_some(),
        proxy: true,
        ws_bind: "127.0.0.1:8790".parse().unwrap(),
        proxy_bind: t.proxy,
        admin_bind: t.admin,
        rules_path: "/nonexistent".into(),
    })
    .await
}

async fn wait_admin(t: &Tunnel, want_ready: bool) {
    for _ in 0..80 {
        let st = status(t).await;
        if st.proxy_up && st.ready == want_ready {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "admin never reached ready={want_ready}: {:?}",
        status(t).await
    );
}

/// A CONNECT's status and its `X-Box-Egress-Reason`, and with a 200, what came back for
/// `payload`.
struct Outcome {
    status: u16,
    reason: Option<String>,
    body: Vec<u8>,
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

async fn connect(proxy: std::net::SocketAddr, target: &str, payload: &[u8]) -> Outcome {
    let mut stream = TcpStream::connect(proxy).await.expect("dial proxy");
    let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let (head, mut rest) = read_http_head(&mut stream).await;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let reason = head
        .split("\r\n")
        .find_map(|l| l.strip_prefix("X-Box-Egress-Reason: "))
        .map(str::to_string);
    if status == 200 {
        stream.write_all(payload).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
            .await
            .expect("read timeout")
            .unwrap();
        rest.extend_from_slice(&buf[..n]);
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut rest)).await;
    }
    Outcome {
        status,
        reason,
        body: rest,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_exit_with_no_client_fails_closed_and_says_which_503_it_is() {
    let (_echo, echo) = echo_server().await;
    let t = start_tunnel(Some(every_site(Exit::Tunnel))).await;
    wait_admin(&t, false).await;
    let got = connect(t.proxy, &echo.to_string(), b"hi").await;
    assert_eq!(got.status, 503, "enabled but no client must fail closed");
    assert_eq!(got.reason.as_deref(), Some("no-client"));
    assert!(String::from_utf8_lossy(&got.body).contains("no client attached"));
    let st = status(&t).await;
    assert!(st.enabled);
    assert!(!st.ready);
    assert!(!st.can_browse);
    assert_eq!(st.counts.no_client, 1);
    assert_eq!(st.counts.direct, 0, "never a direct dial instead");
}

/// A raw HTTP request to the proxy, returning the status line's code and reason header.
async fn raw(proxy: std::net::SocketAddr, request: &[u8]) -> (u16, Option<String>) {
    let mut s = TcpStream::connect(proxy).await.expect("proxy connect");
    s.write_all(request).await.expect("write");
    let (head, _) = read_http_head(&mut s).await;
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code");
    let reason = head
        .split("\r\n")
        .find_map(|l| l.strip_prefix("X-Box-Egress-Reason: "))
        .map(str::to_string);
    (code, reason)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_http_is_refused_whichever_the_exit() {
    let t = start_tunnel(Some(every_site(Exit::Tunnel))).await;
    wait_admin(&t, false).await;
    let _client = attach(t.ws.unwrap(), Some(every_site(Exit::Tunnel)));
    wait_admin(&t, true).await;
    let request = b"GET http://example.com/private HTTP/1.1\r\nHost: example.com\r\n\r\n";
    assert_eq!(
        raw(t.proxy, request).await,
        (405, Some("plain-http".into()))
    );
    t.rules.set(Some(every_site(Exit::Direct)));
    assert_eq!(
        raw(t.proxy, request).await,
        (405, Some("plain-http".into()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_roundtrip_via_client() {
    let (_echo, echo) = echo_server().await;
    let t = start_tunnel(Some(every_site(Exit::Tunnel))).await;
    wait_admin(&t, false).await;
    let _client = attach(t.ws.unwrap(), Some(every_site(Exit::Tunnel)));
    wait_admin(&t, true).await;

    let got = connect(t.proxy, &echo.to_string(), b"hello-via-relay").await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.body, b"hello-via-relay",
        "bytes must round-trip through the laptop relay"
    );
    let st = status(&t).await;
    assert!(st.can_browse);
    assert_eq!(st.counts.tunneled, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_bearer_is_a_401_told_by_status_and_held_before_it_is_told() {
    let t = start_tunnel(Some(every_site(Exit::Tunnel))).await;
    let mut cfg = client_cfg(t.ws.unwrap(), None);
    cfg.bearer = "totally-wrong-token-that-is-also-long".to_string();
    let started = std::time::Instant::now();
    let err = client::run_once(&cfg)
        .await
        .expect_err("wrong bearer must fail");
    assert_eq!(err, box_egress::ClientError::Unauthorized);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "a wrong bearer is held before its 401"
    );
    assert!(!status(&t).await.ready);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_static_relay_hosts_list_still_applies_on_top_of_the_rules() {
    let (_echo, echo) = echo_server().await;
    let t = start(
        true,
        Some(every_site(Exit::Tunnel)),
        DestinationPolicy::default(),
        Allowlist::parse("only.example"),
        None,
    )
    .await;
    let _client = attach(t.ws.unwrap(), Some(every_site(Exit::Tunnel)));
    wait_admin(&t, true).await;
    let got = connect(t.proxy, &echo.to_string(), b"x").await;
    assert_eq!(got.status, 403);
    assert_eq!(got.reason.as_deref(), Some("relay-hosts"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_exit_dials_from_the_box_by_the_same_rules() {
    let (_echo, echo) = echo_server().await;
    let port = echo.port();
    let t = start(
        false,
        Some(rules(
            Exit::Direct,
            Mode::Listed,
            &["localhost"],
            &["blocked.test"],
        )),
        loopback_destination(),
        Allowlist::any(),
        None,
    )
    .await;
    wait_admin(&t, false).await;

    let allowed = connect(t.proxy, &format!("localhost:{port}"), b"direct-bytes").await;
    assert_eq!(allowed.status, 200);
    assert_eq!(allowed.body, b"direct-bytes");

    let blocked = connect(t.proxy, &format!("blocked.test:{port}"), b"x").await;
    assert_eq!(
        (blocked.status, blocked.reason.as_deref()),
        (403, Some("blocked"))
    );
    let unlisted = connect(t.proxy, &format!("elsewhere.test:{port}"), b"x").await;
    assert_eq!(
        (unlisted.status, unlisted.reason.as_deref()),
        (403, Some("not-listed"))
    );
    let bare = connect(t.proxy, &echo.to_string(), b"x").await;
    assert_eq!(
        (bare.status, bare.reason.as_deref()),
        (403, Some("not-listed")),
        "a bare address matches no name in listed mode"
    );
    let st = status(&t).await;
    assert!(!st.enabled, "no tunnel on this box");
    assert!(st.can_browse);
    assert_eq!(st.counts.direct, 1);
    assert_eq!(st.counts.blocked, 1);
    assert_eq!(st.counts.not_listed, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_exit_refuses_private_destinations_by_the_resolved_address() {
    let (_echo, echo) = echo_server().await;
    let t = start(
        false,
        Some(every_site(Exit::Direct)),
        // The shipped policy, with every port so only the address decides.
        DestinationPolicy::default().with_ports(Some(Vec::new())),
        Allowlist::any(),
        None,
    )
    .await;
    // `localhost` is a name, allowed by the rules; it resolves to loopback, refused after DNS.
    let got = connect(t.proxy, &format!("localhost:{}", echo.port()), b"x").await;
    assert_eq!(
        (got.status, got.reason.as_deref()),
        (403, Some("destination"))
    );
    let got = connect(t.proxy, &echo.to_string(), b"x").await;
    assert_eq!(
        (got.status, got.reason.as_deref()),
        (403, Some("destination"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_rules_refuses_every_site_whichever_the_box() {
    let (_echo, echo) = echo_server().await;
    for with_tunnel in [false, true] {
        let t = start(
            with_tunnel,
            None,
            loopback_destination(),
            Allowlist::any(),
            None,
        )
        .await;
        if let Some(ws) = t.ws {
            let _client = attach(ws, Some(every_site(Exit::Tunnel)));
        }
        let got = connect(t.proxy, &format!("localhost:{}", echo.port()), b"x").await;
        assert_eq!((got.status, got.reason.as_deref()), (403, Some("no-rules")));
        let st = status(&t).await;
        assert!(st.rules.is_none());
        assert!(!st.can_browse);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_rules_on_a_box_without_the_tunnel_fail_closed() {
    let (_echo, echo) = echo_server().await;
    let t = start(
        false,
        Some(every_site(Exit::Tunnel)),
        loopback_destination(),
        Allowlist::any(),
        None,
    )
    .await;
    let got = connect(t.proxy, &format!("localhost:{}", echo.port()), b"x").await;
    assert_eq!(
        (got.status, got.reason.as_deref()),
        (503, Some("no-client"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operators_client_judges_by_the_rules_again() {
    // The box's copy allows every site; the client's, the authority for what leaves the
    // operator's network, blocks one. The client's refusal is what the browser gets.
    let (_echo, echo) = echo_server().await;
    let t = start_tunnel(Some(every_site(Exit::Tunnel))).await;
    let _client = attach(
        t.ws.unwrap(),
        Some(rules(Exit::Tunnel, Mode::All, &[], &["localhost"])),
    );
    wait_admin(&t, true).await;
    let got = connect(t.proxy, &format!("localhost:{}", echo.port()), b"x").await;
    assert_eq!(got.status, 502);
    assert!(
        String::from_utf8_lossy(&got.body).contains("blocked by this computer's rules"),
        "{}",
        String::from_utf8_lossy(&got.body)
    );
    // And a client with no rules refuses everything.
    drop(_client);
    wait_admin(&t, false).await;
    let _bare = attach(t.ws.unwrap(), None);
    wait_admin(&t, true).await;
    let got = connect(t.proxy, &format!("localhost:{}", echo.port()), b"x").await;
    assert_eq!(got.status, 502);
    assert!(String::from_utf8_lossy(&got.body).contains("no rules"));
}

async fn reload(t: &Tunnel) -> (u16, String) {
    let got = box_common::local_request(
        &t.admin.to_string(),
        "POST",
        "/v1/rules/reload",
        Duration::from_secs(2),
        4096,
    )
    .await
    .unwrap();
    (got.status, String::from_utf8_lossy(&got.body).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_change_at_runtime_from_the_file_and_a_bad_file_refuses_everything() {
    let (_echo, echo) = echo_server().await;
    let dir = std::env::temp_dir().join(format!("egress-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rules.json");
    let mut first = every_site(Exit::Direct);
    first.version = 41;
    std::fs::write(&path, first.to_json()).unwrap();
    let t = start(
        false,
        None,
        loopback_destination(),
        Allowlist::any(),
        Some(path.clone()),
    )
    .await;
    wait_admin(&t, false).await;
    assert_eq!(status(&t).await.rules.unwrap().version, 41);
    let target = format!("localhost:{}", echo.port());
    assert_eq!(connect(t.proxy, &target, b"x").await.status, 200);

    let mut second = rules(Exit::Direct, Mode::All, &[], &["localhost"]);
    second.version = 42;
    std::fs::write(&path, second.to_json()).unwrap();
    let reloaded = reload(&t).await;
    assert_eq!(reloaded, (200, "{\"version\":42}".to_string()));
    let got = connect(t.proxy, &target, b"x").await;
    assert_eq!(got.reason.as_deref(), Some("blocked"));

    std::fs::write(
        &path,
        br#"{"v":1,"exit":"direct","mode":"all","block":["*"]}"#,
    )
    .unwrap();
    let (code, _) = reload(&t).await;
    assert_eq!(code, 422);
    let st = status(&t).await;
    assert!(st.rules.is_none(), "a set that didn't read is not kept");
    assert!(st.rules_error.is_some());
    let got = connect(t.proxy, &target, b"x").await;
    assert_eq!(got.reason.as_deref(), Some("no-rules"));
    std::fs::remove_dir_all(&dir).unwrap();
}
