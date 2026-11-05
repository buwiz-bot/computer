//! Thin host gateway for buwiz-computer (`box-host`).
//!
//! This process does **not** run inference. It advertises box identity,
//! capabilities (exec, files, desktop, chrome, cua, egress_tunnel), and readiness of
//! `box-exec` (and the X display when desktop is required). Heap: same
//! mimalloc feature as `box-exec`.

pub mod active_tab;
pub mod keeper;

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use box_chrome::{probe_chrome, ChromeConfig, ChromeStatus};
use box_common::{
    bearer_token, container_local_host, container_local_http_url, cors_layer, echo_request_id,
    tokens_equal, ApiError, BoxConfig, GLOBAL_ALLOCATOR, PROTOCOL_VERSION,
};
use box_cua::CuaConfig;
use box_desktop::{slots_dir, DesktopConfig, DesktopStatus, ScreenList, WindowList, VIEWER_KIND};
use box_egress_tunnel::{probe_egress, EgressConfig, EgressStatus};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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
    pub cua: CuaConfig,
    pub egress: EgressConfig,
    pub shutdown: Arc<Notify>,
    pub shutting_down: Arc<AtomicBool>,
    pub started_at: Instant,
}

impl AppState {
    pub fn from_config(config: &BoxConfig) -> Self {
        Self {
            box_id: config.box_id.clone(),
            token: config.host_token.clone(),
            workspace: config.workspace.display().to_string(),
            exec_url: config.exec_url.trim_end_matches('/').to_string(),
            exec_bind: config.exec_bind,
            host_bind: config.host_bind,
            desktop: DesktopConfig::from_env(),
            chrome: ChromeConfig::from_env(),
            cua: CuaConfig::from_env(),
            egress: EgressConfig::from_env(),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        }
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
    cua: Capability,
    egress_tunnel: Capability,
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
    let state = AppState::from_config(&config);
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

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn ready(State(state): State<AppState>) -> Result<Json<ReadyResponse>, ApiError> {
    let exec_ready = probe_exec_health(&state.exec_url).await;
    let desktop_ready = state.desktop.probe_display();
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
    Ok(Json(ReadyResponse {
        status: "ready",
        service: "box-host",
        exec_ready: true,
        desktop_ready,
        viewer_ready: state.desktop.viewer_ready(),
    }))
}

async fn info(State(state): State<AppState>) -> Json<InfoResponse> {
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
                ready: state.desktop.probe_display(),
            },
            viewer: ViewerCapability {
                kind: VIEWER_KIND,
                port: state.desktop.viewer_port(),
                enabled: state.desktop.enabled,
                ready: state.desktop.viewer_ready(),
            },
            chrome: {
                let st = probe_chrome(&state.chrome);
                Capability {
                    enabled: st.enabled,
                    ready: st.ready,
                }
            },
            cua: Capability {
                enabled: state.cua.enabled,
                ready: state.cua.capability_ready(),
            },
            egress_tunnel: {
                let st = probe_egress(&state.egress);
                Capability {
                    enabled: st.enabled,
                    ready: st.ready,
                }
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

async fn desktop(State(state): State<AppState>) -> Json<DesktopStatus> {
    let host = container_local_host(state.host_bind);
    Json(DesktopStatus::from_config(&state.desktop, &host))
}

/// The box's own screen and every Bot's, with each one's viewer port and state.
async fn screens(State(state): State<AppState>) -> Json<ScreenList> {
    Json(ScreenList::from_config(&state.desktop, &slots_dir()))
}

async fn chrome(State(state): State<AppState>) -> Json<ChromeStatus> {
    Json(probe_chrome(&state.chrome))
}

async fn egress(State(state): State<AppState>) -> Json<EgressStatus> {
    Json(probe_egress(&state.egress))
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
    Json(active_tab::read(&active_tab::file_for(
        on.display.as_deref(),
    )))
}

async fn windows(State(state): State<AppState>) -> Json<WindowList> {
    Json(WindowList::from_config(&state.desktop))
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
        desktop_ready: state.desktop.probe_display(),
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
        desktop_ready: state.desktop.probe_display(),
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

/// Tiny HTTP/1.1 probe so box-host does not need an HTTP client crate.
async fn probe_exec_health(exec_url: &str) -> bool {
    let Some(addr) = host_port_from_url(exec_url) else {
        return false;
    };
    let Ok(Ok(mut stream)) =
        tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(&addr)).await
    else {
        return false;
    };
    let req = format!("GET /v1/health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = vec![0u8; 256];
    let Ok(n) = stream.read(&mut buf).await else {
        return false;
    };
    let text = String::from_utf8_lossy(&buf[..n]);
    text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200")
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
            cua: CuaConfig::disabled(),
            egress: EgressConfig::disabled(),
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
        assert!(body["cdp"].is_null());
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
