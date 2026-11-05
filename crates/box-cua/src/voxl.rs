//! The screen's voxl, from inside the box: `/wait` and `/record`.
//!
//! voxl already knows when a screen changes (XDamage) and already encodes it, so an agent can ask
//! it when the screen settled and for an MP4 of what happened, whether or not anyone watches. It
//! sits behind its own password. `box-exec` holds that password and reaches voxl on the box's
//! loopback, so an exec child and a model never need it.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use box_desktop::viewer_port;
use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::{CuaConfig, CuaError};

/// Longest a connection to voxl on the box's own loopback may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Past a wait's own timeout: voxl answers at its timeout, so this is only for a voxl that hung.
const WAIT_MARGIN: Duration = Duration::from_secs(5);
/// voxl's default wait.
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
/// voxl muxes a whole recording before it answers `/record/stop`.
const RECORD_TIMEOUT: Duration = Duration::from_secs(60);

/// `POST /v1/cua/wait`. The fields are voxl's `/wait` parameters; times are in milliseconds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// Return once the screen has been still this long (voxl's default is 300).
    pub idle: Option<u64>,
    /// Return at the first change instead of when the screen goes still.
    #[serde(default)]
    pub change: bool,
    /// Count changes after this `seq`, so one that landed before the request is not missed.
    pub since: Option<u64>,
    /// Only watch this rectangle: `x,y,w,h` in desktop pixels.
    pub region: Option<String>,
    /// Give up after this long (voxl's default is 10000).
    pub timeout: Option<u64>,
}

impl WaitRequest {
    fn query(&self) -> Result<String, CuaError> {
        let mut parts = Vec::new();
        if self.change {
            parts.push("change".to_string());
        }
        if let Some(idle) = self.idle {
            parts.push(format!("idle={idle}"));
        }
        if let Some(since) = self.since {
            parts.push(format!("since={since}"));
        }
        if let Some(region) = &self.region {
            let numbers = region.split(',').map(str::parse::<u32>);
            if numbers.clone().count() != 4 || numbers.clone().any(|n| n.is_err()) {
                return Err(CuaError::Invalid("region must be x,y,w,h".into()));
            }
            parts.push(format!("region={region}"));
        }
        if let Some(timeout) = self.timeout {
            parts.push(format!("timeout={timeout}"));
        }
        Ok(parts.join("&"))
    }
}

/// voxl's answer to `/wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitResponse {
    pub changed: bool,
    pub timed_out: bool,
    /// Pass as `since` on a later wait.
    pub seq: u64,
    pub waited_ms: u64,
}

/// A recording under way. Its id is the screen's voxl's, so it stops on the same screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStarted {
    pub id: u64,
}

/// `POST /v1/cua/record/stop`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordStopRequest {
    pub id: u64,
}

/// How `box-exec` reaches a screen's voxl. Its `Debug` never prints the password.
#[derive(Clone, Default)]
pub struct ViewerClient {
    password: Option<Arc<str>>,
}

impl fmt::Debug for ViewerClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewerClient")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ViewerClient {
    pub fn new(password: Option<String>) -> Self {
        Self {
            password: password.map(Arc::from),
        }
    }

    /// When the screen settles or changes. See [`WaitRequest`].
    pub async fn wait(
        &self,
        config: &CuaConfig,
        req: &WaitRequest,
    ) -> Result<WaitResponse, CuaError> {
        self.wait_on(port_of(config)?, req).await
    }

    /// Start recording the screen. Stop it with [`ViewerClient::record_stop`].
    pub async fn record_start(&self, config: &CuaConfig) -> Result<RecordStarted, CuaError> {
        self.record_start_on(port_of(config)?).await
    }

    /// Stop recording and write the MP4 to `dest`. Returns its size in bytes.
    pub async fn record_stop(
        &self,
        config: &CuaConfig,
        id: u64,
        dest: &Path,
    ) -> Result<u64, CuaError> {
        self.record_stop_on(port_of(config)?, id, dest).await
    }

    async fn wait_on(&self, port: u16, req: &WaitRequest) -> Result<WaitResponse, CuaError> {
        let timeout = req.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let limit = Duration::from_millis(timeout).saturating_add(WAIT_MARGIN);
        let path = format!("/wait?{}", req.query()?);
        tokio::time::timeout(limit, async {
            let response = self.send(port, Method::GET, &path).await?;
            json_body(response).await
        })
        .await
        .map_err(|_| CuaError::Viewer("the screen's voxl did not answer in time".into()))?
    }

    async fn record_start_on(&self, port: u16) -> Result<RecordStarted, CuaError> {
        tokio::time::timeout(RECORD_TIMEOUT, async {
            let response = self.send(port, Method::POST, "/record/start").await?;
            json_body(response).await
        })
        .await
        .map_err(|_| CuaError::Viewer("the screen's voxl did not answer in time".into()))?
    }

    async fn record_stop_on(&self, port: u16, id: u64, dest: &Path) -> Result<u64, CuaError> {
        tokio::time::timeout(RECORD_TIMEOUT, async {
            let path = format!("/record/stop?id={id}");
            let response = self.send(port, Method::POST, &path).await?;
            save_body(checked(response).await?, dest).await
        })
        .await
        .map_err(|_| CuaError::Viewer("the screen's voxl did not answer in time".into()))?
    }

    async fn send(
        &self,
        port: u16,
        method: Method,
        path: &str,
    ) -> Result<Response<Incoming>, CuaError> {
        let password = self
            .password
            .as_deref()
            .ok_or_else(|| CuaError::Viewer("this box has no viewer password".into()))?;
        let address = ("127.0.0.1", port);
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| CuaError::Viewer(format!("voxl on port {port} did not answer")))?
            .map_err(|err| CuaError::Viewer(format!("voxl on port {port} is down: {err}")))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(viewer_error)?;
        tokio::spawn(connection);
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, format!("127.0.0.1:{port}"))
            .header(header::AUTHORIZATION, format!("Bearer {password}"))
            .body(Empty::<Bytes>::new())
            // The error would describe the header it refused, so it is not passed on.
            .map_err(|_| CuaError::Viewer("the viewer password is not a valid header".into()))?;
        sender.send_request(request).await.map_err(viewer_error)
    }
}

fn port_of(config: &CuaConfig) -> Result<u16, CuaError> {
    if !config.enabled {
        return Err(CuaError::Disabled);
    }
    viewer_port(&config.display)
        .ok_or_else(|| CuaError::Invalid(format!("display {} has no viewer", config.display)))
}

fn viewer_error(err: hyper::Error) -> CuaError {
    CuaError::Viewer(format!("the screen's voxl failed: {err}"))
}

/// The response, if voxl said yes. Anything else becomes an error that says what it said.
async fn checked(response: Response<Incoming>) -> Result<Response<Incoming>, CuaError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.into_body().collect().await.map_err(viewer_error)?;
    let said = String::from_utf8_lossy(&body.to_bytes()).into_owned();
    Err(match status {
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::CONFLICT => {
            CuaError::Invalid(format!("voxl refused: {said}"))
        }
        _ => CuaError::Viewer(format!("voxl answered {status}: {said}")),
    })
}

async fn json_body<T: serde::de::DeserializeOwned>(
    response: Response<Incoming>,
) -> Result<T, CuaError> {
    let body = checked(response)
        .await?
        .into_body()
        .collect()
        .await
        .map_err(viewer_error)?;
    serde_json::from_slice(&body.to_bytes())
        .map_err(|err| CuaError::Viewer(format!("voxl's answer did not parse: {err}")))
}

/// Stream the body to `dest` through a `.part` file, so a half-written MP4 never has its name.
async fn save_body(response: Response<Incoming>, dest: &Path) -> Result<u64, CuaError> {
    let io_error = |err: std::io::Error| CuaError::Tool(format!("recording not saved: {err}"));
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(io_error)?;
    }
    let part = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&part).await.map_err(io_error)?;
    let mut body = response.into_body();
    let mut bytes = 0u64;
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.map_err(viewer_error)?.into_data() {
            file.write_all(&data).await.map_err(io_error)?;
            bytes += data.len() as u64;
        }
    }
    file.flush().await.map_err(io_error)?;
    tokio::fs::rename(&part, dest).await.map_err(io_error)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// A voxl that takes one request, answers `reply`, and hands back what it was sent.
    async fn fake_voxl(reply: &'static str) -> (u16, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
            stream.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8(seen).unwrap()
        });
        (port, task)
    }

    fn client() -> ViewerClient {
        ViewerClient::new(Some("viewer-pass".into()))
    }

    const WAITED: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 60\r\nconnection: close\r\n\r\n{\"changed\":true,\"timed_out\":false,\"seq\":812,\"waited_ms\":340}";

    #[tokio::test]
    async fn wait_passes_its_fields_to_voxl_with_the_password() {
        let (port, seen) = fake_voxl(WAITED).await;
        let req = WaitRequest {
            change: true,
            since: Some(811),
            region: Some("0,0,800,600".into()),
            timeout: Some(1000),
            ..WaitRequest::default()
        };
        let waited = client().wait_on(port, &req).await.unwrap();
        assert_eq!(
            waited,
            WaitResponse {
                changed: true,
                timed_out: false,
                seq: 812,
                waited_ms: 340
            }
        );
        let seen = seen.await.unwrap();
        assert!(
            seen.starts_with("GET /wait?change&since=811&region=0,0,800,600&timeout=1000 HTTP/1.1"),
            "{seen}"
        );
        assert!(seen
            .to_ascii_lowercase()
            .contains("authorization: bearer viewer-pass"));
    }

    #[tokio::test]
    async fn a_region_is_four_numbers() {
        for region in ["1,2,3", "a,b,c,d", "1,2,3,4,5", "1,2,3,4&idle=0"] {
            let req = WaitRequest {
                region: Some(region.into()),
                ..WaitRequest::default()
            };
            assert!(matches!(req.query(), Err(CuaError::Invalid(_))), "{region}");
        }
    }

    #[tokio::test]
    async fn no_password_is_no_viewer() {
        let err = ViewerClient::default()
            .wait_on(1, &WaitRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(err, CuaError::Viewer(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_voxl_that_is_down_is_a_viewer_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = client()
            .wait_on(port, &WaitRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(err, CuaError::Viewer(_)), "{err:?}");
    }

    #[tokio::test]
    async fn record_start_reads_the_id() {
        let (port, seen) = fake_voxl(
            "HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{\"id\":3}",
        )
        .await;
        let started = client().record_start_on(port).await.unwrap();
        assert_eq!(started, RecordStarted { id: 3 });
        assert!(seen
            .await
            .unwrap()
            .starts_with("POST /record/start HTTP/1.1"));
    }

    #[tokio::test]
    async fn record_stop_writes_the_mp4_under_its_name() {
        let (port, seen) =
            fake_voxl("HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nmp4!")
                .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("recordings/one.mp4");
        let bytes = client().record_stop_on(port, 7, &dest).await.unwrap();
        assert_eq!(bytes, 4);
        assert_eq!(std::fs::read(&dest).unwrap(), b"mp4!");
        assert!(!dest.with_extension("part").exists());
        assert!(seen
            .await
            .unwrap()
            .starts_with("POST /record/stop?id=7 HTTP/1.1"));
    }

    #[tokio::test]
    async fn an_unknown_recording_is_the_callers_error() {
        let (port, _seen) =
            fake_voxl("HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await;
        let dir = tempfile::tempdir().unwrap();
        let err = client()
            .record_stop_on(port, 9, &dir.path().join("x.mp4"))
            .await
            .unwrap_err();
        assert!(matches!(err, CuaError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn debug_never_prints_the_password() {
        assert!(!format!("{:?}", client()).contains("viewer-pass"));
    }
}
