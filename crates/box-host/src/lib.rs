//! Thin host gateway for buwiz-computer (`box-host`).
//!
//! This process does **not** run inference. It advertises box identity,
//! capabilities (exec, files, desktop, chrome, cua, egress_tunnel), and readiness of
//! `box-exec` (and the X display when desktop is required). Heap: same
//! mimalloc feature as `box-exec`.

pub mod active_tab;
pub mod egress_rules;
pub mod keeper;

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use box_chrome::{probe_chrome, ChromeConfig, ChromeStatus};
use box_common::{
    bearer_token, container_local_host, container_local_http_url, cors_layer, echo_request_id,
    tokens_equal, ApiError, BoxConfig, ConfigError, GLOBAL_ALLOCATOR, PROTOCOL_VERSION,
};
use box_cua::CuaConfig;
use box_desktop::{slots_dir, DesktopConfig, DesktopStatus, ScreenList, WindowList, VIEWER_KIND};
use box_egress_tunnel::{probe_egress, EgressConfig, EgressStatus};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tower_http::trace::TraceLayer;

#[derive(Clone, Debug)]
pub struct AppState {
    pub box_id: String,
    pub token: String,
    pub workspace: String,
    pub exec_url: String,
    pub exec_bind: SocketAddr,
    pub host_bind: SocketAddr,
    pub desktop: DesktopConfig,
    pub chrome: ChromeConfig,
    /// `/v1/browser/*` is served ([`box_common::browser_layer_on`]).
    pub browser: bool,
    pub cua: CuaConfig,
    pub egress: EgressConfig,
    pub rules_turn: egress_rules::RulesTurn,
    pub shutdown: Arc<Notify>,
    pub shutting_down: Arc<AtomicBool>,
    pub started_at: Instant,
}

impl AppState {
    /// Fails when an egress bind is set and malformed.
    pub fn from_config(config: &BoxConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            box_id: config.box_id.clone(),
            token: config.host_token.clone(),
            workspace: config.workspace.display().to_string(),
            exec_url: config.exec_url.trim_end_matches('/').to_string(),
            exec_bind: config.exec_bind,
            host_bind: config.host_bind,
            desktop: DesktopConfig::from_env(),
            chrome: ChromeConfig::from_env(),
            browser: box_common::browser_layer_on(),
            cua: CuaConfig::from_env(),
            egress: EgressConfig::from_env()?,
            rules_turn: Default::default(),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        })
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    service: &'static str,
    exec_ready: bool,
    desktop_ready: bool,
    viewer_ready: bool,
}

#[derive(Serialize)]
struct InfoResponse {
    box_id: String,
    service: &'static str,
    protocol: &'static str,
    version: VersionInfo,
    capabilities: Capabilities,
    endpoints: Endpoints,
    workspace: String,
}

#[derive(Serialize)]
struct VersionInfo {
    box_host: &'static str,
    box_exec: &'static str,
    protocol: &'static str,
}

#[derive(Serialize)]
struct Capability {
    enabled: bool,
    ready: bool,
}

/// What serves the box's screens to a person. The viewer password is never here:
/// whoever started the box already holds it.
#[derive(Serialize)]
struct ViewerCapability {
    kind: &'static str,
    port: u16,
    enabled: bool,
    ready: bool,
}

#[derive(Serialize)]
struct Capabilities {
    exec: Capability,
    files: Capability,
    desktop: Capability,
    viewer: ViewerCapability,
    chrome: Capability,
    /// `/v1/browser/*`: on with `BOX_BROWSER` and `BOX_DESKTOP`, whatever `BOX_CHROME` says. Ready
    /// once the desktop is; a screen with no browser on a pipe still answers
    /// `503 browser_unavailable`.
    browser: Capability,
    cua: Capability,
    egress_tunnel: Capability,
    /// The browser goes through the box's proxy, under the computer's rules
    /// (`BOX_EGRESS_PROXY`, or the tunnel). Ready once the proxy answers with rules set.
    egress_proxy: Capability,
}

#[derive(Serialize)]
struct Endpoints {
    exec: String,
    host: String,
    /// Always `container-local`. Not a public connect URL for SDKs.
    scope: &'static str,
}

pub fn app(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/ready", get(ready))
        .route("/v1/info", get(info))
        .route("/v1/desktop", get(desktop))
        .route("/v1/desktop/windows", get(windows))
        .route("/v1/screens", get(screens))
        .route("/v1/chrome", get(chrome))
        .route("/v1/chrome/active-tab", get(active_tab_now))
        .route("/v1/egress", get(egress))
        .route(
            "/v1/egress/rules",
            get(egress_rules::get_rules).merge(put(egress_rules::put_rules)),
        )
        .route("/v1/busy", get(busy))
        .route("/v1/shutdown", post(shutdown))
        .route("/v1/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/v1/health", get(health))
        .merge(protected)
        .layer(middleware::from_fn(echo_request_id))
        .with_state(state)
}

pub fn router(state: AppState) -> Router {
    app(state)
        .layer(TraceLayer::new_for_http())
        .layer(cors_layer())
}

pub async fn serve(config: BoxConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bind = config.host_bind;
    let state = AppState::from_config(&config)?;
    if state.desktop.enabled && !on_path("xdpyinfo", &std::env::var_os("PATH").unwrap_or_default())
    {
        tracing::warn!(
            "xdpyinfo is not on PATH: the desktop is on, but its display will never be ready"
        );
    }
    let shutdown = state.shutdown.clone();
    let app = router(state);
    tracing::info!(
        %bind,
        box_id = %config.box_id,
        allocator = GLOBAL_ALLOCATOR,
        "box-host listening"
    );
    let listener = TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

/// Whether `program` is an executable file in one of `path`'s folders.
fn on_path(program: &str, path: &std::ffi::OsStr) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).any(|dir| {
        std::fs::metadata(dir.join(program))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Run a probe that may block (a loopback connect) off the async workers.
async fn blocking<T: Send + 'static>(probe: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(probe).await {
        Ok(value) => value,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}

/// Whether the display answers. Its X helper is bounded by [`box_desktop::TOOL_TIMEOUT`].
async fn display_ready(desktop: &DesktopConfig) -> bool {
    desktop.probe_display().await
}

/// Whether the display answers, and whether its viewer does.
async fn desktop_probe(desktop: &DesktopConfig) -> (bool, bool) {
    let viewer = desktop.clone();
    tokio::join!(
        desktop.probe_display(),
        blocking(move || viewer.viewer_ready())
    )
}

async fn ready(State(state): State<AppState>) -> Result<Json<ReadyResponse>, ApiError> {
    let exec_ready = probe_exec_health(&state.exec_url).await;
    let (desktop_ready, viewer_ready) = desktop_probe(&state.desktop).await;
    if !exec_ready {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "box-exec is not reachable",
        ));
    }
    if state.desktop.required && !desktop_ready {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "desktop display is not up",
        ));
    }
    // A box with the proxy isn't ready until the proxy answers, so whoever waits for ready can
    // change the rules at once. Its browser refuses every site until then either way.
    if state.egress.proxy && !probe_egress(&state.egress).await.proxy_up {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "the browser's network proxy is not up",
        ));
    }
    Ok(Json(ReadyResponse {
        status: "ready",
        service: "box-host",
        exec_ready: true,
        desktop_ready,
        viewer_ready,
    }))
}

async fn info(State(state): State<AppState>) -> Json<InfoResponse> {
    let ((desktop_ready, viewer_ready), egress) =
        tokio::join!(desktop_probe(&state.desktop), probe_egress(&state.egress));
    Json(InfoResponse {
        box_id: state.box_id.clone(),
        service: "box-host",
        protocol: PROTOCOL_VERSION,
        version: VersionInfo {
            box_host: env!("CARGO_PKG_VERSION"),
            box_exec: env!("CARGO_PKG_VERSION"),
            protocol: PROTOCOL_VERSION,
        },
        capabilities: Capabilities {
            exec: Capability {
                enabled: true,
                ready: true,
            },
            files: Capability {
                enabled: true,
                ready: true,
            },
            desktop: Capability {
                enabled: state.desktop.enabled,
                ready: desktop_ready,
            },
            viewer: ViewerCapability {
                kind: VIEWER_KIND,
                port: state.desktop.viewer_port(),
                enabled: state.desktop.enabled,
                ready: viewer_ready,
            },
            chrome: {
                let st = probe_chrome(&state.chrome);
                Capability {
                    enabled: st.enabled,
                    ready: st.ready,
                }
            },
            browser: browser_capability(state.browser, desktop_ready),
            cua: Capability {
                enabled: state.cua.enabled,
                ready: state.cua.capability_ready(),
            },
            egress_tunnel: Capability {
                enabled: egress.enabled,
                ready: egress.ready,
            },
            egress_proxy: Capability {
                enabled: egress.proxy_enabled,
                ready: egress.proxy_up && egress.rules.is_some(),
            },
        },
        endpoints: Endpoints {
            exec: container_local_http_url(state.exec_bind),
            host: container_local_http_url(state.host_bind),
            scope: "container-local",
        },
        workspace: state.workspace.clone(),
    })
}

/// `capabilities.browser`: the layer on (`BOX_BROWSER` and `BOX_DESKTOP`, whatever `BOX_CHROME`
/// says), and ready once the desktop is. Ready says nothing of any one screen's browser.
fn browser_capability(on: bool, desktop_ready: bool) -> Capability {
    Capability {
        enabled: on,
        ready: on && desktop_ready,
    }
}

async fn desktop(State(state): State<AppState>) -> Json<DesktopStatus> {
    let host = container_local_host(state.host_bind);
    Json(DesktopStatus::from_config(&state.desktop, &host).await)
}

/// The box's own screen and every Bot's, with each one's viewer port and state.
async fn screens(State(state): State<AppState>) -> Json<ScreenList> {
    Json(blocking(move || ScreenList::from_config(&state.desktop, &slots_dir())).await)
}

async fn chrome(State(state): State<AppState>) -> Json<ChromeStatus> {
    Json(probe_chrome(&state.chrome))
}

async fn egress(State(state): State<AppState>) -> Json<EgressStatus> {
    let mut st = probe_egress(&state.egress).await;
    st.rules_origin =
        egress_rules::origin(&state, st.rules.as_ref().map(|r| r.version)).map(str::to_owned);
    Json(st)
}

/// `?display=:N`: one of the box's other screens (a Bot's own, `box-screen`).
#[derive(Debug, Default, serde::Deserialize)]
struct OnScreen {
    display: Option<String>,
}

/// The page in front of Chromium, as the box's extension last reported it (`url: null` when
/// nothing has), on the box's own screen or the one named. Read before a saved login is typed;
/// see [`active_tab`].
async fn active_tab_now(Query(on): Query<OnScreen>) -> Json<active_tab::ActiveTab> {
    Json(active_tab::read_screen(on.display.as_deref()))
}

async fn windows(State(state): State<AppState>) -> Json<WindowList> {
    Json(WindowList::from_config(&state.desktop).await)
}

#[derive(Serialize)]
struct HostBusy {
    busy: bool,
    shutting_down: bool,
    exec_ready: bool,
    desktop_ready: bool,
    chrome_ready: bool,
}

#[derive(Serialize)]
struct HostShutdown {
    ok: bool,
    service: &'static str,
}

#[derive(Serialize)]
struct HostMetrics {
    service: &'static str,
    uptime_ms: u64,
    desktop_ready: bool,
    chrome_ready: bool,
}

async fn busy(State(state): State<AppState>) -> Json<HostBusy> {
    let chrome = probe_chrome(&state.chrome);
    Json(HostBusy {
        busy: false,
        shutting_down: state
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed),
        exec_ready: probe_exec_health(&state.exec_url).await,
        desktop_ready: display_ready(&state.desktop).await,
        chrome_ready: chrome.ready,
    })
}

async fn shutdown(State(state): State<AppState>) -> Json<HostShutdown> {
    state
        .shutting_down
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let notify = state.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        notify.notify_waiters();
    });
    Json(HostShutdown {
        ok: true,
        service: "box-host",
    })
}

async fn metrics(State(state): State<AppState>) -> Json<HostMetrics> {
    Json(HostMetrics {
        service: "box-host",
        uptime_ms: state.started_at.elapsed().as_millis() as u64,
        desktop_ready: display_ready(&state.desktop).await,
        chrome_ready: probe_chrome(&state.chrome).ready,
    })
}

async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let presented = bearer_token(request.headers()).ok_or_else(ApiError::unauthorized)?;
    if !tokens_equal(presented, &state.token) {
        return Err(ApiError::unauthorized());
    }
    Ok(next.run(request).await)
}

/// Longest the exec health probe may take, connect to last byte.
const EXEC_PROBE_DEADLINE: Duration = Duration::from_secs(2);
/// A health answer is a few bytes.
const EXEC_PROBE_CAP: usize = 16 * 1024;

/// `GET /v1/health` on box-exec, bounded in time and size so box-host needs no HTTP client crate.
async fn probe_exec_health(exec_url: &str) -> bool {
    let Some(addr) = host_port_from_url(exec_url) else {
        return false;
    };
    box_common::local_get(&addr, "/v1/health", EXEC_PROBE_DEADLINE, EXEC_PROBE_CAP)
        .await
        .is_ok_and(|answer| answer.status == 200)
}

fn host_port_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    if hostport.contains(':') {
        Some(hostport.to_string())
    } else {
        Some(format!("{hostport}:80"))
    }
}

async fn shutdown_signal(notify: Arc<Notify>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = notify.notified() => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tool_is_on_path_only_as_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("xdpyinfo");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let path = std::env::join_paths(["/nonexistent", dir.path().to_str().unwrap()]).unwrap();
        assert!(!super::on_path("xdpyinfo", &path));
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(super::on_path("xdpyinfo", &path));
        assert!(!super::on_path(
            "xdpyinfo",
            std::ffi::OsStr::new("/nonexistent")
        ));
    }

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    fn state() -> AppState {
        AppState {
            box_id: "test-box".into(),
            token: "host-secret".into(),
            workspace: "/workspace".into(),
            exec_url: "http://127.0.0.1:1337".into(),
            exec_bind: "0.0.0.0:1337".parse().unwrap(),
            host_bind: "0.0.0.0:1340".parse().unwrap(),
            desktop: DesktopConfig::disabled(),
            chrome: ChromeConfig::disabled(),
            browser: false,
            cua: CuaConfig::disabled(),
            egress: EgressConfig::disabled(),
            rules_turn: Default::default(),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        }
    }

    async fn send(req: Request<Body>) -> (StatusCode, Value) {
        let response = app(state()).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn health_public() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
        assert!(body.get("service").is_none());
        assert!(body.get("version").is_none());
    }

    #[tokio::test]
    async fn ready_rejects_missing_auth() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn ready_with_token_reaches_probe() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/ready")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_ne!(status, StatusCode::UNAUTHORIZED);
        assert!(
            status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
            "ready with token should probe exec, got {status} {body}"
        );
        if status == StatusCode::OK {
            assert_eq!(body["exec_ready"], true);
        } else {
            assert_eq!(body["error"]["code"], "not_ready");
        }
    }

    #[tokio::test]
    async fn info_rejects_missing_auth() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn info_rejects_wrong_token() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/info")
                .header("authorization", "Bearer nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn the_browser_capability_is_on_with_chrome_off_and_ready_with_the_desktop() {
        let mut on = state();
        on.browser = true;
        let response = app(on)
            .oneshot(
                Request::builder()
                    .uri("/v1/info")
                    .header("authorization", "Bearer host-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["capabilities"]["chrome"]["enabled"], false);
        assert_eq!(body["capabilities"]["browser"]["enabled"], true);
        assert_eq!(
            body["capabilities"]["browser"]["ready"], false,
            "no desktop up here"
        );
        let up = browser_capability(true, true);
        assert!(up.enabled && up.ready, "ready tracks the desktop");
        let off = browser_capability(false, true);
        assert!(!off.enabled && !off.ready);
    }

    #[tokio::test]
    async fn info_ok_with_token() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/info")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["box_id"], "test-box");
        assert_eq!(body["capabilities"]["exec"]["enabled"], true);
        assert_eq!(body["capabilities"]["exec"]["ready"], true);
        assert_eq!(body["capabilities"]["files"]["enabled"], true);
        assert_eq!(body["capabilities"]["desktop"]["enabled"], false);
        assert_eq!(body["capabilities"]["desktop"]["ready"], false);
        assert_eq!(body["capabilities"]["viewer"]["kind"], "voxl");
        assert_eq!(body["capabilities"]["viewer"]["port"], 6901);
        assert_eq!(body["capabilities"]["viewer"]["enabled"], false);
        assert_eq!(body["capabilities"]["viewer"]["ready"], false);
        assert_eq!(body["capabilities"]["chrome"]["enabled"], false);
        assert_eq!(body["capabilities"]["chrome"]["ready"], false);
        assert_eq!(body["capabilities"]["browser"]["enabled"], false);
        assert_eq!(body["capabilities"]["browser"]["ready"], false);
        assert_eq!(body["capabilities"]["cua"]["enabled"], false);
        assert_eq!(body["capabilities"]["cua"]["ready"], false);
        assert_eq!(body["capabilities"]["egress_tunnel"]["enabled"], false);
        assert_eq!(body["capabilities"]["egress_tunnel"]["ready"], false);
        assert_eq!(body["endpoints"]["exec"], "http://127.0.0.1:1337");
        assert_eq!(body["endpoints"]["host"], "http://127.0.0.1:1340");
        assert_eq!(body["endpoints"]["scope"], "container-local");
    }

    #[tokio::test]
    async fn egress_ok_when_disabled() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/egress")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["ready"], false);
        assert_eq!(body["client_attached"], false);
        assert_eq!(body["protocol"], "box-egress-v1");
    }

    async fn send_to(state: AppState, req: Request<Body>) -> (StatusCode, Value) {
        let response = app(state).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn rules_put(body: &str) -> Request<Body> {
        Request::builder()
            .method("PUT")
            .uri("/v1/egress/rules")
            .header("authorization", "Bearer host-secret")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn rules_put_owned(body: String) -> Request<Body> {
        Request::builder()
            .method("PUT")
            .uri("/v1/egress/rules")
            .header("authorization", "Bearer host-secret")
            .body(Body::from(body))
            .unwrap()
    }

    fn egress_get() -> Request<Body> {
        Request::builder()
            .uri("/v1/egress")
            .header("authorization", "Bearer host-secret")
            .body(Body::empty())
            .unwrap()
    }

    fn rules_get() -> Request<Body> {
        Request::builder()
            .uri("/v1/egress/rules")
            .header("authorization", "Bearer host-secret")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn rules_are_refused_on_a_box_without_the_proxy() {
        let (status, body) = send(rules_put(r#"{"v":1,"exit":"direct","mode":"all"}"#)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "egress_proxy_off");
        let (status, body) = send(rules_get()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "egress_proxy_off");
        let unauthenticated = Request::builder()
            .method("PUT")
            .uri("/v1/egress/rules")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(send(unauthenticated).await.0, StatusCode::UNAUTHORIZED);
    }

    /// box-host against a real proxy on loopback, its rules file in a temp folder.
    async fn with_proxy(
        tunnel: bool,
    ) -> (AppState, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use box_egress_tunnel::{server, TunnelServerConfig};
        let dir = tempfile::tempdir().unwrap();
        let rules_path = dir.path().join("rules.json");
        let cfg = TunnelServerConfig {
            tunnel: None,
            proxy_bind: "127.0.0.1:0".parse().unwrap(),
            admin_bind: "127.0.0.1:0".parse().unwrap(),
            allowlist: box_egress::Allowlist::any(),
            destination: box_egress::destination::DestinationPolicy::default(),
            rules_path: Some(rules_path.clone()),
            rules: box_egress::RulesCell::default(),
        };
        let binds = server::ServerBinds::bind(&cfg).await.unwrap();
        let proxy_bind = binds.proxy.local_addr().unwrap();
        let admin_bind = binds.admin.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = server::run_with_binds(cfg, binds).await;
        });
        let mut st = state();
        st.egress = EgressConfig {
            enabled: tunnel,
            proxy: true,
            ws_bind: "127.0.0.1:8790".parse().unwrap(),
            proxy_bind,
            admin_bind,
            rules_path,
        };
        (st, dir, task)
    }

    #[tokio::test]
    async fn rules_set_at_runtime_are_in_force_when_the_put_answers() {
        let (st, _dir, task) = with_proxy(false).await;
        let (status, body) = send_to(st.clone(), rules_get()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "egress_rules_unset");
        let (_, info) = send_to(
            st.clone(),
            Request::builder()
                .uri("/v1/info")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(info["capabilities"]["egress_proxy"]["enabled"], true);
        assert_eq!(info["capabilities"]["egress_proxy"]["ready"], false);

        let (status, body) = send_to(
            st.clone(),
            rules_put(
                r#"{"v":1,"version":9,"exit":"direct","mode":"listed","allow":["Example.com"],"block":["ads.example.com"]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true);
        assert_eq!(body["rules"]["version"], 9);
        assert_eq!(body["rules"]["allow"], 1);

        let (status, body) = send_to(st.clone(), rules_get()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["allow"], serde_json::json!(["example.com"]));
        assert_eq!(body["mode"], "listed");

        let (_, egress) = send_to(
            st.clone(),
            Request::builder()
                .uri("/v1/egress")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(egress["proxy_up"], true);
        assert_eq!(egress["can_browse"], true);
        assert_eq!(egress["rules"]["block"], 1);
        assert!(
            !egress.to_string().contains("example.com"),
            "a status names no site: {egress}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn two_puts_at_once_each_told_the_truth_and_one_set_stands() {
        let (st, _dir, task) = with_proxy(false).await;
        let put = |v: u64| {
            rules_put_owned(format!(
                r#"{{"v":1,"version":{v},"exit":"direct","mode":"all","block":["b{v}.test"]}}"#
            ))
        };
        let (a, b) = tokio::join!(send_to(st.clone(), put(10)), send_to(st.clone(), put(11)));
        assert_eq!(
            (a.0, b.0),
            (StatusCode::OK, StatusCode::OK),
            "{} {}",
            a.1,
            b.1
        );
        let (_, body) = send_to(st.clone(), rules_get()).await;
        let v = body["version"].as_u64().unwrap();
        assert!(v == 10 || v == 11, "{body}");
        assert_eq!(
            body["block"],
            serde_json::json!([format!("b{v}.test")]),
            "the file and what's in force are one set"
        );
        let (_, egress) = send_to(st.clone(), egress_get()).await;
        assert_eq!(egress["rules"]["version"].as_u64(), Some(v));
        assert_eq!(egress["rules_origin"], "set");
        task.abort();
    }

    /// box-host against an admin port that answers every reload with `answer`.
    async fn with_admin(answer: &'static str) -> (AppState, tempfile::TempDir) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin_bind = admin.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = admin.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(answer.as_bytes()).await;
            }
        });
        let mut st = state();
        st.egress = EgressConfig {
            enabled: false,
            proxy: true,
            ws_bind: "127.0.0.1:8790".parse().unwrap(),
            proxy_bind: "127.0.0.1:8791".parse().unwrap(),
            admin_bind,
            rules_path: dir.path().join("rules.json"),
        };
        (st, dir)
    }

    #[tokio::test]
    async fn a_set_the_proxy_refuses_or_supersedes_leaves_the_file_as_it_was() {
        let before = r#"{"v":1,"version":2,"exit":"direct","mode":"all"}"#;
        for (answer, status, code) in [
            ("HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 47\r\n\r\n{\"error\":\"bad_rules\",\"message\":\"unreadable\"}", StatusCode::UNPROCESSABLE_ENTITY, "rules_refused"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"version\":999}", StatusCode::CONFLICT, "rules_superseded"),
        ] {
            let (st, dir) = with_admin(answer).await;
            std::fs::write(dir.path().join("rules.json"), before).unwrap();
            let (got, body) = send_to(st, rules_put(r#"{"v":1,"version":7,"exit":"direct","mode":"all","block":["x.test"]}"#)).await;
            assert_eq!(got, status, "{body}");
            assert_eq!(body["error"]["code"], code);
            assert_eq!(std::fs::read_to_string(dir.path().join("rules.json")).unwrap(), before, "never ahead of the proxy");
        }
        // no proxy answering at all: nothing changed either, and said so
        let (mut st, dir) = with_admin("").await;
        st.egress.admin_bind = "127.0.0.1:1".parse().unwrap();
        std::fs::write(dir.path().join("rules.json"), before).unwrap();
        let (got, body) = send_to(
            st,
            rules_put(r#"{"v":1,"version":8,"exit":"direct","mode":"all"}"#),
        )
        .await;
        // no proxy to ask: what's in force isn't known, and the answer says that, not "unchanged"
        assert_eq!(got, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "rules_state_unknown");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("rules.json")).unwrap(),
            before
        );
    }

    /// box-host against an admin port that answers `GET /v1/rules` with `in_force` after `get_ms`,
    /// and every reload after `reload_ms`, with the version asked for in force.
    async fn with_slow_admin(
        in_force: u64,
        get_ms: u64,
        reload_ms: u64,
    ) -> (AppState, tempfile::TempDir) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin_bind = admin.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = admin.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let (wait, body) = if req.starts_with("GET /v1/rules") {
                        (
                            get_ms,
                            format!(
                                r#"{{"version":{in_force},"exit":"direct","mode":"all","allow":0,"block":1}}"#
                            ),
                        )
                    } else {
                        (reload_ms, format!(r#"{{"version":{in_force}}}"#))
                    };
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    let _ = s
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        let mut st = state();
        st.egress = EgressConfig {
            enabled: false,
            proxy: true,
            ws_bind: "127.0.0.1:8790".parse().unwrap(),
            proxy_bind: "127.0.0.1:8791".parse().unwrap(),
            admin_bind,
            rules_path: dir.path().join("rules.json"),
        };
        (st, dir)
    }

    #[tokio::test]
    async fn a_reload_that_timed_out_after_it_took_is_found_out_and_answered_truly() {
        let before = r#"{"v":1,"version":2,"exit":"direct","mode":"all"}"#;
        let new = r#"{"v":1,"version":7,"exit":"direct","mode":"all","block":["x.test"]}"#;
        // the reload's answer comes too late, but the proxy has the new set in force
        let (st, dir) = with_slow_admin(7, 0, 3000).await;
        std::fs::write(dir.path().join("rules.json"), before).unwrap();
        let (got, body) = send_to(st, rules_put(new)).await;
        assert_eq!(got, StatusCode::OK, "{body}");
        assert!(
            std::fs::read_to_string(dir.path().join("rules.json"))
                .unwrap()
                .contains("x.test"),
            "the file is the set in force"
        );
        // and when the proxy can't be asked at all: unknown, said so, the file the last known set
        let (st, dir) = with_slow_admin(7, 3000, 3000).await;
        std::fs::write(dir.path().join("rules.json"), before).unwrap();
        let (got, body) = send_to(st, rules_put(new)).await;
        assert_eq!(got, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["code"], "rules_state_unknown");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("rules.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn a_staged_file_with_no_version_is_said_to_be_staged() {
        let (st, dir, task) = with_proxy(false).await;
        std::fs::write(
            dir.path().join("rules.json"),
            r#"{"v":1,"exit":"direct","mode":"all"}"#,
        )
        .unwrap();
        let reload = box_common::local_request(
            &st.egress.admin_bind.to_string(),
            "POST",
            "/v1/rules/reload",
            Duration::from_secs(2),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(reload.status, 200);
        let (_, egress) = send_to(st, egress_get()).await;
        assert_eq!(egress["rules_origin"], "staged", "{egress}");
        task.abort();
    }

    #[tokio::test]
    async fn rules_the_box_started_with_are_said_to_be_staged() {
        let (st, dir, task) = with_proxy(false).await;
        std::fs::write(
            dir.path().join("rules.json"),
            r#"{"v":1,"version":77,"exit":"direct","mode":"all"}"#,
        )
        .unwrap();
        let reload = box_common::local_request(
            &st.egress.admin_bind.to_string(),
            "POST",
            "/v1/rules/reload",
            Duration::from_secs(2),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(reload.status, 200);
        let (_, egress) = send_to(st, egress_get()).await;
        assert_eq!(
            (
                egress["rules"]["version"].as_u64(),
                egress["rules_origin"].as_str()
            ),
            (Some(77), Some("staged")),
            "{egress}"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_bad_set_is_refused_whole_and_changes_nothing() {
        let (st, _dir, task) = with_proxy(false).await;
        let good = r#"{"v":1,"version":3,"exit":"direct","mode":"all","block":["a.test"]}"#;
        assert_eq!(send_to(st.clone(), rules_put(good)).await.0, StatusCode::OK);
        for bad in [
            r#"{"v":1,"exit":"direct","mode":"all","block":["*"]}"#,
            r#"{"v":1,"exit":"direct","mode":"all","allow":[""]}"#,
            r#"{"v":1,"exit":"direct","mode":"everything"}"#,
            r#"not json"#,
        ] {
            let (status, body) = send_to(st.clone(), rules_put(bad)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
            assert_eq!(body["error"]["code"], "invalid_rules");
        }
        let (status, body) = send_to(
            st.clone(),
            rules_put(r#"{"v":1,"exit":"tunnel","mode":"all"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "egress_tunnel_off");
        let (_, body) = send_to(st.clone(), rules_get()).await;
        assert_eq!(body["version"], 3);
        assert_eq!(body["block"], serde_json::json!(["a.test"]));
        task.abort();
    }

    #[tokio::test]
    async fn a_box_with_the_proxy_is_not_ready_while_the_proxy_is_down() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A box-exec that answers its health route.
        let exec = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let exec_addr = exec.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = exec.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await;
            }
        });
        let ready = || {
            Request::builder()
                .uri("/v1/ready")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap()
        };
        let (mut st, _dir, task) = with_proxy(false).await;
        st.exec_url = format!("http://{exec_addr}");
        assert_eq!(send_to(st.clone(), ready()).await.0, StatusCode::OK);
        task.abort();
        st.egress.admin_bind = "127.0.0.1:1".parse().unwrap();
        let (status, body) = send_to(st, ready()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "not_ready");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("network proxy"));
    }

    #[tokio::test]
    async fn chrome_ok_when_disabled() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/chrome")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["running"], false);
        assert_eq!(body["ready"], false);
    }

    #[tokio::test]
    async fn desktop_rejects_missing_auth() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/desktop")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn desktop_ok_when_disabled() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/desktop")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["available"], false);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["ready"], false);
        assert_eq!(body["display"], ":1");
        assert_eq!(body["viewer"]["kind"], "voxl");
        assert_eq!(body["viewer"]["port"], 6901);
        assert_eq!(body["viewer"]["path"], "/");
        assert!(body.get("vnc").is_none());
    }

    #[tokio::test]
    async fn screens_rejects_missing_auth() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/screens")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[tokio::test]
    async fn screens_empty_when_the_desktop_is_off() {
        let (status, body) = send(
            Request::builder()
                .uri("/v1/screens")
                .header("authorization", "Bearer host-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "screens": [] }));
    }

    #[test]
    fn parse_exec_url() {
        assert_eq!(
            host_port_from_url("http://127.0.0.1:1337"),
            Some("127.0.0.1:1337".into())
        );
        assert_eq!(
            host_port_from_url("http://127.0.0.1:1337/v1/health"),
            Some("127.0.0.1:1337".into())
        );
    }
}
