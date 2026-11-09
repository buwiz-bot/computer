//! Connect-only HTTP client for a running buwiz-computer guest.
//!
//! The caller starts the container and passes the **published** exec URL, host
//! URL, and bearer token. This crate never runs Docker and never replaces those
//! URLs with `/v1/info.endpoints` (those are container-local listen addresses).

mod error;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Uri};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use error::Error;

type Https = hyper_rustls::HttpsConnector<HttpConnector>;
type HttpClient = Client<Https, Full<Bytes>>;

fn build_http() -> HttpClient {
    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    Client::builder(TokioExecutor::new()).build(https)
}

fn trim_slash(url: impl Into<String>) -> String {
    let url = url.into();
    url.trim().trim_end_matches('/').to_string()
}

/// Typed client. Construct with [`BuwizComputer::connect`].
#[derive(Clone, Debug)]
pub struct BuwizComputer {
    exec_url: String,
    host_url: String,
    token: String,
    http: HttpClient,
    /// The screen the computer-use and page-in-front calls go to: `None` is the box's own;
    /// `:N` is a Bot's own screen (`box-screen`). See [`BuwizComputer::on`].
    display: Option<String>,
}

impl BuwizComputer {
    /// Connect to an already-running guest. URLs are used as given.
    pub fn connect(
        exec_url: impl Into<String>,
        host_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, Error> {
        let exec_url = trim_slash(exec_url);
        let host_url = trim_slash(host_url);
        let token = token.into();
        if exec_url.is_empty() || host_url.is_empty() || token.is_empty() {
            return Err(Error::Connect(
                "exec URL, host URL, and token are required".into(),
            ));
        }
        let http = build_http();
        Ok(Self {
            exec_url,
            host_url,
            token,
            http,
            display: None,
        })
    }

    /// This client on another of the box's screens (`:N`, a Bot's own, started by
    /// `box-screen up`): its clicks, keys, screenshots and page-in-front reports are that
    /// screen's. Commands and files are the box's whatever the screen.
    #[must_use]
    pub fn on(&self, display: &str) -> Self {
        Self {
            display: Some(display.to_string()),
            ..self.clone()
        }
    }

    /// `/v1/cua/<path>` on the exec URL, on this client's screen.
    fn cua(&self, path: &str) -> String {
        format!("{}/v1/cua/{path}{}", self.exec_url, self.screen_query('?'))
    }

    /// `<sep>display=:N` (escaped) for a screen other than the box's own; empty otherwise.
    fn screen_query(&self, sep: char) -> String {
        match &self.display {
            Some(display) => format!("{sep}display={}", display.replace(':', "%3A")),
            None => String::new(),
        }
    }

    pub fn exec_url(&self) -> &str {
        &self.exec_url
    }

    pub fn host_url(&self) -> &str {
        &self.host_url
    }

    pub async fn health_exec(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/health", self.exec_url), None, false)
            .await
    }

    pub async fn health_host(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/health", self.host_url), None, false)
            .await
    }

    pub async fn ready(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/ready", self.host_url), None, true)
            .await
    }

    /// Capability inventory. Advertised endpoint URLs are container-local;
    /// this client keeps using the URLs passed to [`connect`](Self::connect).
    pub async fn info(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/info", self.host_url), None, true)
            .await
    }

    pub async fn exec(&self, request: &ExecRequest) -> Result<ExecResponse, Error> {
        self.send_json(
            "POST",
            &format!("{}/v1/exec", self.exec_url),
            Some(json!(request)),
            true,
        )
        .await
    }

    pub async fn exec_status(&self, id: &str) -> Result<ExecResponse, Error> {
        self.send_json(
            "GET",
            &format!("{}/v1/exec/{}", self.exec_url, urlencoding(id)),
            None,
            true,
        )
        .await
    }

    /// Stop a running exec and its whole process group.
    pub async fn exec_cancel(&self, id: &str) -> Result<ExecCancelResponse, Error> {
        self.send_json(
            "DELETE",
            &format!("{}/v1/exec/{}", self.exec_url, urlencoding(id)),
            None,
            true,
        )
        .await
    }

    pub async fn files_get(&self, path: &str, encoding: Option<&str>) -> Result<Value, Error> {
        let mut url = format!("{}/v1/files?path={}", self.exec_url, urlencoding(path));
        if let Some(encoding) = encoding {
            url.push_str("&encoding=");
            url.push_str(&urlencoding(encoding));
        }
        self.send_json("GET", &url, None, true).await
    }

    pub async fn files_put(&self, request: &FilePutRequest) -> Result<FilePutResponse, Error> {
        self.send_json(
            "PUT",
            &format!("{}/v1/files", self.exec_url),
            Some(json!(request)),
            true,
        )
        .await
    }

    pub async fn files_delete(
        &self,
        path: &str,
        recursive: bool,
    ) -> Result<FileDeleteResponse, Error> {
        let mut url = format!("{}/v1/files?path={}", self.exec_url, urlencoding(path));
        if recursive {
            url.push_str("&recursive=true");
        }
        self.send_json("DELETE", &url, None, true).await
    }

    pub async fn files_mkdir(&self, request: &MkdirRequest) -> Result<MkdirResponse, Error> {
        self.send_json(
            "POST",
            &format!("{}/v1/files/mkdir", self.exec_url),
            Some(json!(request)),
            true,
        )
        .await
    }

    pub async fn files_rename(&self, from: &str, to: &str) -> Result<Value, Error> {
        self.send_json(
            "POST",
            &format!("{}/v1/files/rename", self.exec_url),
            Some(json!({ "from": from, "to": to })),
            true,
        )
        .await
    }

    pub async fn files_get_raw(&self, path: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}/v1/files/raw?path={}", self.exec_url, urlencoding(path));
        let (status, bytes, _) = self
            .send("GET", &url, None, true, "application/octet-stream")
            .await?;
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes);
            return Err(Error::from_status(status, &text));
        }
        Ok(bytes.to_vec())
    }

    pub async fn files_put_raw(&self, path: &str, body: Vec<u8>) -> Result<FilePutResponse, Error> {
        let url = format!("{}/v1/files/raw?path={}", self.exec_url, urlencoding(path));
        let (status, bytes, _) = self
            .send_bytes("PUT", &url, Some(body), true, "application/json")
            .await?;
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes);
            return Err(Error::from_status(status, &text));
        }
        serde_json::from_slice(&bytes).map_err(|err| Error::Transport(err.to_string()))
    }

    pub async fn desktop(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/desktop", self.host_url), None, true)
            .await
    }

    /// `GET /v1/screens`: every screen of the box with its display, viewer port and state.
    pub async fn screens(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/screens", self.host_url), None, true)
            .await
    }

    /// `POST /v1/cua/wait`: when the screen settles or changes. `body` is voxl's `/wait` fields
    /// (`idle`, `change`, `since`, `region`, `timeout`).
    pub async fn cua_wait(&self, body: Value) -> Result<Value, Error> {
        let url = format!("{}/v1/cua/wait{}", self.exec_url, self.screen_query('?'));
        self.send_json("POST", &url, Some(body), true).await
    }

    /// `POST /v1/cua/record/start`: `{id}`.
    pub async fn cua_record_start(&self) -> Result<Value, Error> {
        let url = format!(
            "{}/v1/cua/record/start{}",
            self.exec_url,
            self.screen_query('?')
        );
        self.send_json("POST", &url, None, true).await
    }

    /// `POST /v1/cua/record/stop`: `{path, bytes}`, the MP4 under the workspace.
    pub async fn cua_record_stop(&self, id: u64) -> Result<Value, Error> {
        let url = format!(
            "{}/v1/cua/record/stop{}",
            self.exec_url,
            self.screen_query('?')
        );
        self.send_json("POST", &url, Some(json!({ "id": id })), true)
            .await
    }

    pub async fn chrome(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/chrome", self.host_url), None, true)
            .await
    }

    /// `GET /v1/chrome/active-tab`: `{url, atMs?}`, the page in front of the box's Chromium as
    /// its extension last reported it; `url` is null when nothing has.
    pub async fn active_tab(&self) -> Result<Value, Error> {
        self.send_json(
            "GET",
            &format!(
                "{}/v1/chrome/active-tab{}",
                self.host_url,
                self.screen_query('?')
            ),
            None,
            true,
        )
        .await
    }

    pub async fn egress(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/egress", self.host_url), None, true)
            .await
    }

    pub async fn windows(&self) -> Result<Value, Error> {
        self.send_json(
            "GET",
            &format!("{}/v1/desktop/windows", self.host_url),
            None,
            true,
        )
        .await
    }

    pub async fn busy(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/busy", self.exec_url), None, true)
            .await
    }

    pub async fn metrics(&self) -> Result<Value, Error> {
        self.send_json("GET", &format!("{}/v1/metrics", self.exec_url), None, true)
            .await
    }

    pub async fn shutdown(&self, host: bool) -> Result<Value, Error> {
        let url = if host {
            format!("{}/v1/shutdown", self.host_url)
        } else {
            format!("{}/v1/shutdown", self.exec_url)
        };
        self.send_json("POST", &url, None, true).await
    }

    pub async fn exec_stream(&self, request: &ExecRequest) -> Result<String, Error> {
        let (status, bytes, _) = self
            .send(
                "POST",
                &format!("{}/v1/exec/stream", self.exec_url),
                Some(json!(request)),
                true,
                "application/x-ndjson",
            )
            .await?;
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes);
            return Err(Error::from_status(status, &text));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn screenshot(&self) -> Result<ScreenshotResponse, Error> {
        self.send_json("POST", &self.cua("screenshot"), None, true)
            .await
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>, Error> {
        let (status, bytes, _) = self
            .send(
                "POST",
                &format!(
                    "{}/v1/cua/screenshot?format=png{}",
                    self.exec_url,
                    self.screen_query('&')
                ),
                None,
                true,
                "image/png",
            )
            .await?;
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes);
            return Err(Error::from_status(status, &text));
        }
        Ok(bytes.to_vec())
    }

    pub async fn click(&self, x: i32, y: i32, button: Option<u8>) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("click"),
            Some(json!({ "x": x, "y": y, "button": button })),
            true,
        )
        .await
    }

    pub async fn double_click(&self, x: i32, y: i32, button: Option<u8>) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("double-click"),
            Some(json!({ "x": x, "y": y, "button": button })),
            true,
        )
        .await
    }

    pub async fn move_pointer(&self, x: i32, y: i32) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("move"),
            Some(json!({ "x": x, "y": y })),
            true,
        )
        .await
    }

    pub async fn drag(
        &self,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        button: Option<u8>,
    ) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("drag"),
            Some(json!({ "x1": x1, "y1": y1, "x2": x2, "y2": y2, "button": button })),
            true,
        )
        .await
    }

    pub async fn press(&self, x: i32, y: i32, button: Option<u8>) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("press"),
            Some(json!({ "x": x, "y": y, "button": button })),
            true,
        )
        .await
    }

    pub async fn release(
        &self,
        x: Option<i32>,
        y: Option<i32>,
        button: Option<u8>,
        path: Option<Vec<(i32, i32)>>,
    ) -> Result<CuaOk, Error> {
        let path = path.map(|pts| {
            pts.into_iter()
                .map(|(x, y)| json!({ "x": x, "y": y }))
                .collect::<Vec<_>>()
        });
        self.send_json(
            "POST",
            &self.cua("release"),
            Some(json!({ "x": x, "y": y, "button": button, "path": path })),
            true,
        )
        .await
    }

    pub async fn type_text(&self, text: &str) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("type"),
            Some(json!({ "text": text })),
            true,
        )
        .await
    }

    pub async fn key(&self, key: &str) -> Result<CuaOk, Error> {
        self.key_action(key, None).await
    }

    pub async fn key_action(&self, key: &str, action: Option<&str>) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("key"),
            Some(json!({ "key": key, "action": action })),
            true,
        )
        .await
    }

    pub async fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<CuaOk, Error> {
        self.send_json(
            "POST",
            &self.cua("scroll"),
            Some(json!({ "x": x, "y": y, "dx": dx, "dy": dy })),
            true,
        )
        .await
    }

    /// Run many CUA steps in one request. The guest validates the whole plan
    /// before moving the pointer.
    pub async fn recipe(&self, request: &Value) -> Result<Value, Error> {
        self.send_json("POST", &self.cua("recipe"), Some(request.clone()), true)
            .await
    }

    /// `POST /v1/cua/tree`: the screen's accessibility tree (AT-SPI), each node with a `ref`,
    /// role, name, states, bounds in screen pixels and actions. `body` takes `scope`
    /// (`focused_window`, the default, or `desktop`), `max_depth`, `max_nodes`, `timeout_ms` and
    /// `include_hidden`; `{}` for the defaults.
    pub async fn cua_tree(&self, body: Value) -> Result<Value, Error> {
        self.send_json("POST", &self.cua("tree"), Some(body), true)
            .await
    }

    /// `POST /v1/cua/element`: `action` (`press`, `click`, `focus` or `set_text`, with `text`) on
    /// the node `reference` names in the screen's latest tree. A ref from an older tree is
    /// refused with `stale_ref`.
    pub async fn cua_element(
        &self,
        reference: &str,
        action: &str,
        text: Option<&str>,
    ) -> Result<Value, Error> {
        let mut body = json!({ "ref": reference, "action": action });
        if let Some(text) = text {
            body["text"] = json!(text);
        }
        self.send_json("POST", &self.cua("element"), Some(body), true)
            .await
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        url: &str,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T, Error> {
        let (status, bytes, _) = self
            .send(method, url, body, auth, "application/json")
            .await?;
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes);
            return Err(Error::from_status(status, &text));
        }
        serde_json::from_slice(&bytes).map_err(|err| Error::Transport(err.to_string()))
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<Value>,
        auth: bool,
        accept: &str,
    ) -> Result<(u16, Bytes, String), Error> {
        let uri: Uri = url
            .parse()
            .map_err(|err: hyper::http::uri::InvalidUri| Error::Connect(err.to_string()))?;
        let payload = match &body {
            Some(value) => {
                serde_json::to_vec(value).map_err(|err| Error::Transport(err.to_string()))?
            }
            None => Vec::new(),
        };
        let mut builder = Request::builder().method(method).uri(uri);
        if auth {
            builder = builder.header("authorization", format!("Bearer {}", self.token));
        }
        builder = builder.header("accept", accept);
        builder = builder.header("x-request-id", next_request_id());
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let request = builder
            .body(Full::new(Bytes::from(payload)))
            .map_err(|err| Error::Transport(err.to_string()))?;
        let response = self
            .http
            .request(request)
            .await
            .map_err(|err| Error::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|err| Error::Transport(err.to_string()))?
            .to_bytes();
        Ok((status, bytes, content_type))
    }

    async fn send_bytes(
        &self,
        method: &str,
        url: &str,
        body: Option<Vec<u8>>,
        auth: bool,
        accept: &str,
    ) -> Result<(u16, Bytes, String), Error> {
        let uri: Uri = url
            .parse()
            .map_err(|err: hyper::http::uri::InvalidUri| Error::Connect(err.to_string()))?;
        let payload = body.unwrap_or_default();
        let mut builder = Request::builder().method(method).uri(uri);
        if auth {
            builder = builder.header("authorization", format!("Bearer {}", self.token));
        }
        builder = builder.header("accept", accept);
        builder = builder.header("x-request-id", next_request_id());
        builder = builder.header("content-type", "application/octet-stream");
        let request = builder
            .body(Full::new(Bytes::from(payload)))
            .map_err(|err| Error::Transport(err.to_string()))?;
        let response = self
            .http
            .request(request)
            .await
            .map_err(|err| Error::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|err| Error::Transport(err.to_string()))?
            .to_bytes();
        Ok((status, bytes, content_type))
    }
}

fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    format!("buwiz-computer-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Serialize)]
pub struct ExecRequest {
    pub command: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detach: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pty: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub truncated: bool,
    /// Both pipes reached EOF. `false` means a process the command left
    /// running still holds them and later output was not captured. Defaults to
    /// `true` so a response from an older guest is not read as incomplete.
    #[serde(default = "default_true")]
    pub output_complete: bool,
    pub cwd: String,
    #[serde(default)]
    pub exec_id: String,
    #[serde(default)]
    pub detached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecCancelResponse {
    pub exec_id: String,
    pub cancelled: bool,
}

#[derive(Debug, Serialize)]
pub struct FilePutRequest {
    pub path: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_dirs: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FilePutResponse {
    pub path: String,
    pub bytes_written: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FileDeleteResponse {
    pub path: String,
    pub deleted: bool,
}

#[derive(Debug, Serialize)]
pub struct MkdirRequest {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parents: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MkdirResponse {
    pub path: String,
    pub created: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ScreenshotResponse {
    pub encoding: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub png_base64: String,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CuaOk {
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_requires_all_three() {
        assert!(BuwizComputer::connect("", "http://127.0.0.1:1340", "t").is_err());
        assert!(BuwizComputer::connect("http://127.0.0.1:1337", "", "t").is_err());
        assert!(
            BuwizComputer::connect("http://127.0.0.1:1337", "http://127.0.0.1:1340", "").is_err()
        );
        let box_client =
            BuwizComputer::connect("http://127.0.0.1:1337/", "http://127.0.0.1:1340/", "tok")
                .unwrap();
        assert_eq!(box_client.exec_url(), "http://127.0.0.1:1337");
        assert_eq!(box_client.host_url(), "http://127.0.0.1:1340");
    }

    /// The box's own screen keeps the URLs it always had; a Bot's own screen names its display.
    #[test]
    fn a_client_on_another_screen_names_it_on_every_screen_call() {
        let shared =
            BuwizComputer::connect("http://127.0.0.1:1337", "http://127.0.0.1:1340", "tok")
                .unwrap();
        assert_eq!(shared.cua("click"), "http://127.0.0.1:1337/v1/cua/click");
        let own = shared.on(":3");
        assert_eq!(
            own.cua("click"),
            "http://127.0.0.1:1337/v1/cua/click?display=%3A3"
        );
        assert_eq!(own.screen_query('&'), "&display=%3A3");
        assert_eq!(shared.screen_query('&'), "");
    }
}
