//! Connect-only HTTP client for a running buwiz-computer guest.
//!
//! The caller starts the container and passes the **published** exec URL, host
//! URL, and bearer token. This crate never runs Docker and never replaces those
//! URLs with `/v1/info.endpoints` (those are container-local listen addresses).
//!
//! # Timeouts
//!
//! Every request has a bound on the whole round trip, connect to last body byte: by default
//! [`DEFAULT_TIMEOUT`] (120 s), set with [`BuwizComputer::with_timeout`]. A request that runs
//! out of it is [`Error::Timeout`]. Two kinds of call wait on purpose, and get longer:
//!
//! - [`BuwizComputer::exec`] and [`BuwizComputer::exec_stream`] wait for the command, so a
//!   request with `timeout_ms` is bounded by the larger of the client timeout and `timeout_ms`
//!   plus [`EXEC_MARGIN`] (the guest's SIGTERM-to-SIGKILL grace and the reply). A detached exec
//!   answers at once, and [`BuwizComputer::exec_status`] never waits for the job.
//! - [`BuwizComputer::cua_wait`] waits for the screen, so it is bounded by the larger of the
//!   client timeout and the wait's own `timeout` (voxl's default 10 s) plus [`WAIT_MARGIN`].
//! - [`BuwizComputer::cua_tree`] waits for the walk, so it is bounded by the larger of the client
//!   timeout and its `timeout_ms` (the guest's default 3 s) plus [`TREE_MARGIN`].
//! - [`BuwizComputer::browser_wait`] and [`BuwizComputer::browser_run`] wait for the page or the
//!   goal, so each is bounded by the larger of the client timeout and its `timeout_ms` (the
//!   guest's default 10 s for a wait, 120 s for a run) plus [`BROWSER_MARGIN`].
//!
//! The file calls ([`BuwizComputer::files_get`], [`BuwizComputer::files_put`],
//! [`BuwizComputer::files_get_raw`], [`BuwizComputer::files_put_raw`]) move bytes, so their bound
//! grows with them: the client timeout plus one second for every [`TRANSFER_FLOOR`] bytes sent,
//! and, once the answer's `content-length` is known, one more for every [`TRANSFER_FLOOR`] bytes
//! of it. An answer with no `content-length` is bounded instead by an idle bound: each piece of
//! it must come within the client timeout of the last.
//!
//! A long [`BuwizComputer::recipe`] is bounded by the client timeout like any other call: raise
//! it with [`BuwizComputer::with_timeout`] for a recipe with many waits.

mod error;

use std::time::Duration;

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

/// The bound on a request's whole round trip unless [`BuwizComputer::with_timeout`] says
/// otherwise. The same as the TypeScript SDK's exec and the Python SDK's default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Added to an exec's own `timeout_ms`: the guest's SIGTERM-to-SIGKILL grace (at most 30 s,
/// `BOX_EXEC_KILL_GRACE_MS`) and the reply.
pub const EXEC_MARGIN: Duration = Duration::from_secs(35);

/// Added to a `cua_wait`'s own `timeout`, as the guest adds to it before giving up on voxl.
pub const WAIT_MARGIN: Duration = Duration::from_secs(5);

/// Added to a `cua_tree`'s own `timeout_ms`: connecting to the screen's bus and the reply.
pub const TREE_MARGIN: Duration = Duration::from_secs(5);

/// Added to a `browser_wait`'s or `browser_run`'s own `timeout_ms`: the guest gives the browser
/// 15 s past it before it gives up, and then replies.
pub const BROWSER_MARGIN: Duration = Duration::from_secs(20);

/// The guest's `browser_wait` and `browser_run` when the request names no `timeout_ms`.
const DEFAULT_BROWSER_WAIT: Duration = Duration::from_millis(10_000);
const DEFAULT_BROWSER_RUN: Duration = Duration::from_millis(120_000);

/// The slowest rate a file transfer is given time for, in bytes a second (64 KiB/s): a 10 MiB
/// file gets 160 s on top of the client timeout.
pub const TRANSFER_FLOOR: u64 = 64 * 1024;

/// The guest's tree walk when the request names no `timeout_ms`.
const DEFAULT_TREE: Duration = Duration::from_millis(3000);

/// How long `bytes` take at [`TRANSFER_FLOOR`].
fn transfer_time(bytes: u64) -> Duration {
    Duration::from_millis(bytes.saturating_mul(1000) / TRANSFER_FLOOR)
}

/// How a request's time is bounded.
#[derive(Clone, Copy, Debug)]
enum Bound {
    /// The whole round trip.
    Whole(Duration),
    /// A file transfer: see the crate docs, §Timeouts.
    Transfer,
}

/// voxl's wait when the request names none.
const DEFAULT_WAIT: Duration = Duration::from_secs(10);

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
    /// See the crate docs, §Timeouts.
    timeout: Duration,
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
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// This client with every request bounded by `timeout` instead of [`DEFAULT_TIMEOUT`]. The
    /// exec and screen-wait calls still get at least their own wait plus a margin; see the crate
    /// docs, §Timeouts.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The bound on a request, before any per-call extension.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The bound on an exec of `request`: it waits for the command.
    fn exec_bound(&self, request: &ExecRequest) -> Duration {
        match request.timeout_ms {
            Some(ms) => self
                .timeout
                .max(Duration::from_millis(ms).saturating_add(EXEC_MARGIN)),
            None => self.timeout,
        }
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

    /// Bounded by the client timeout, or by `timeout_ms` plus [`EXEC_MARGIN`] if that is longer.
    pub async fn exec(&self, request: &ExecRequest) -> Result<ExecResponse, Error> {
        self.send_json_within(
            Bound::Whole(self.exec_bound(request)),
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
        self.send_json_within(Bound::Transfer, "GET", &url, None, true)
            .await
    }

    pub async fn files_put(&self, request: &FilePutRequest) -> Result<FilePutResponse, Error> {
        self.send_json_within(
            Bound::Transfer,
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
            .send_within(
                Bound::Transfer,
                "GET",
                &url,
                None,
                true,
                "application/octet-stream",
            )
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
    /// (`idle`, `change`, `since`, `region`, `timeout`). Bounded by the client timeout, or by the
    /// wait's `timeout` plus [`WAIT_MARGIN`] if that is longer.
    pub async fn cua_wait(&self, body: Value) -> Result<Value, Error> {
        let url = format!("{}/v1/cua/wait{}", self.exec_url, self.screen_query('?'));
        let wait = body
            .get("timeout")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_WAIT, Duration::from_millis);
        let bound = self.timeout.max(wait.saturating_add(WAIT_MARGIN));
        self.send_json_within(Bound::Whole(bound), "POST", &url, Some(body), true)
            .await
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

    /// The whole NDJSON stream, once the command is done. Bounded like [`Self::exec`].
    pub async fn exec_stream(&self, request: &ExecRequest) -> Result<String, Error> {
        let (status, bytes, _) = self
            .send_within(
                Bound::Whole(self.exec_bound(request)),
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
    /// (`focused_window`, the default, or `desktop`), `max_depth`, `max_nodes`, `timeout_ms`,
    /// `include_hidden` and `compact`; `{}` for the defaults. Bounded by the client timeout, or by
    /// `timeout_ms` plus [`TREE_MARGIN`] if that is longer.
    pub async fn cua_tree(&self, body: Value) -> Result<Value, Error> {
        let bound = self.tree_bound(&body);
        self.send_json_within(
            Bound::Whole(bound),
            "POST",
            &self.cua("tree"),
            Some(body),
            true,
        )
        .await
    }

    fn tree_bound(&self, body: &Value) -> Duration {
        let walk = body
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_TREE, Duration::from_millis);
        self.timeout.max(walk.saturating_add(TREE_MARGIN))
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

    /// `/v1/browser/<path>` on the exec URL, on this client's screen.
    fn browser(&self, path: &str) -> String {
        format!(
            "{}/v1/browser/{path}{}",
            self.exec_url,
            self.screen_query('?')
        )
    }

    /// `POST /v1/browser/snap`: the current tab's interactive elements, each with an `@eN` ref
    /// good for this snapshot alone, its role, label and rect. `body` takes `max_nodes`; `{}` for
    /// the default. A password field's text never comes.
    pub async fn browser_snap(&self, body: Value) -> Result<Value, Error> {
        self.send_json("POST", &self.browser("snap"), Some(body), true)
            .await
    }

    /// `POST /v1/browser/act`: `{ref, action, text?, confirm?, snapshot?, allow_domains?}`, with
    /// `action` one of `click`, `type`, `select`, `key`, `scroll` or `focus`, through
    /// ultra-instinct's ticketed guard. A click on a label like `Pay*` is refused with
    /// `409 confirm_required` and `error.details.{label, confirm}` until `confirm` brings the token
    /// back.
    pub async fn browser_act(&self, body: Value) -> Result<Value, Error> {
        self.send_json("POST", &self.browser("act"), Some(body), true)
            .await
    }

    /// `POST /v1/browser/navigate`: `{url, allow_domains?}`.
    pub async fn browser_navigate(&self, body: Value) -> Result<Value, Error> {
        self.send_json("POST", &self.browser("navigate"), Some(body), true)
            .await
    }

    /// `POST /v1/browser/tabs`: `{op: list|open|close|switch, tab_id?, url?, allow_domains?}`.
    pub async fn browser_tabs(&self, body: Value) -> Result<Value, Error> {
        self.send_json("POST", &self.browser("tabs"), Some(body), true)
            .await
    }

    /// `POST /v1/browser/wait`: until `text`, `url`, `control`, `gone` or `load` holds, or
    /// `timeout_ms` runs out. Bounded by the client timeout, or by `timeout_ms` plus
    /// [`BROWSER_MARGIN`] if that is longer.
    pub async fn browser_wait(&self, body: Value) -> Result<Value, Error> {
        let bound = self.browser_bound(&body, DEFAULT_BROWSER_WAIT);
        self.send_json_within(
            Bound::Whole(bound),
            "POST",
            &self.browser("wait"),
            Some(body),
            true,
        )
        .await
    }

    /// `POST /v1/browser/shot`: the current tab's viewport as a PNG, `{format, data, bytes,
    /// width, height}` with `data` base64.
    pub async fn browser_shot(&self) -> Result<Value, Error> {
        self.send_json("POST", &self.browser("shot"), Some(json!({})), true)
            .await
    }

    /// `POST /v1/browser/run`: `{goal, max_steps?, timeout_ms?, allow_domains?, confirm?}`.
    /// Instinct runs the loop; the answer's `outcome` is `done`, `blocked`, `abstained` or
    /// `failed`, with the verified `steps`. Bounded by the client timeout, or by `timeout_ms`
    /// plus [`BROWSER_MARGIN`] if that is longer.
    pub async fn browser_run(&self, body: Value) -> Result<Value, Error> {
        let bound = self.browser_bound(&body, DEFAULT_BROWSER_RUN);
        self.send_json_within(
            Bound::Whole(bound),
            "POST",
            &self.browser("run"),
            Some(body),
            true,
        )
        .await
    }

    /// `POST /v1/browser/state`: `{op: save|load, name?}`, this screen's saved login.
    pub async fn browser_state(&self, body: Value) -> Result<Value, Error> {
        self.send_json("POST", &self.browser("state"), Some(body), true)
            .await
    }

    fn browser_bound(&self, body: &Value, default: Duration) -> Duration {
        let own = body
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map_or(default, Duration::from_millis);
        self.timeout.max(own.saturating_add(BROWSER_MARGIN))
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        url: &str,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T, Error> {
        self.send_json_within(Bound::Whole(self.timeout), method, url, body, auth)
            .await
    }

    async fn send_json_within<T: for<'de> Deserialize<'de>>(
        &self,
        limit: Bound,
        method: &str,
        url: &str,
        body: Option<Value>,
        auth: bool,
    ) -> Result<T, Error> {
        let (status, bytes, _) = self
            .send_within(limit, method, url, body, auth, "application/json")
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
        self.send_within(Bound::Whole(self.timeout), method, url, body, auth, accept)
            .await
    }

    async fn send_within(
        &self,
        limit: Bound,
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
        self.round_trip(request, limit).await
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
        self.round_trip(request, Bound::Transfer).await
    }

    /// Send `request` and read the whole answer within `limit`.
    async fn round_trip(
        &self,
        request: Request<Full<Bytes>>,
        limit: Bound,
    ) -> Result<(u16, Bytes, String), Error> {
        use hyper::body::Body;
        use tokio::time::{timeout_at, Instant};

        let started = Instant::now();
        let mut deadline = started
            + match limit {
                Bound::Whole(limit) => limit,
                Bound::Transfer => self
                    .timeout
                    .saturating_add(transfer_time(request.body().size_hint().lower())),
            };
        let out_of_time = |deadline: Instant| Error::Timeout(deadline - started);
        let response = timeout_at(deadline, self.http.request(request))
            .await
            .map_err(|_| out_of_time(deadline))?
            .map_err(|err| Error::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header("content-type").unwrap_or_default();
        let length = header("content-length").and_then(|v| v.parse::<u64>().ok());
        let mut body = response.into_body();
        let idle = match (limit, length) {
            (Bound::Whole(_), _) => None,
            (Bound::Transfer, Some(length)) => {
                deadline += transfer_time(length);
                None
            }
            (Bound::Transfer, None) => Some(self.timeout),
        };
        let Some(idle) = idle else {
            let bytes = timeout_at(deadline, body.collect())
                .await
                .map_err(|_| out_of_time(deadline))?
                .map_err(|err| Error::Transport(err.to_string()))?
                .to_bytes();
            return Ok((status, bytes, content_type));
        };
        let mut bytes = Vec::new();
        loop {
            let frame = tokio::time::timeout(idle, body.frame())
                .await
                .map_err(|_| Error::Timeout(idle))?;
            match frame {
                None => break,
                Some(frame) => {
                    let frame = frame.map_err(|err| Error::Transport(err.to_string()))?;
                    if let Some(data) = frame.data_ref() {
                        bytes.extend_from_slice(data);
                    }
                }
            }
        }
        Ok((status, Bytes::from(bytes), content_type))
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

    fn exec_request(timeout_ms: Option<u64>) -> ExecRequest {
        ExecRequest {
            command: json!(["true"]),
            cwd: None,
            timeout_ms,
            env: None,
            stdin: None,
            detach: None,
            pty: None,
        }
    }

    /// A server that takes the request and then says nothing for `silence`.
    async fn slow_server(silence: Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    tokio::time::sleep(silence).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                        .await;
                });
            }
        });
        url
    }

    /// A server that reads each whole request and then writes `steps`: each is a pause, then
    /// bytes. The connection closes after the last.
    async fn scripted_server(steps: Vec<(Duration, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let steps = steps.clone();
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 65536];
                    loop {
                        let text = String::from_utf8_lossy(&seen).to_ascii_lowercase();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                            if seen.len() >= end + 4 + length {
                                break;
                            }
                        }
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => seen.extend_from_slice(&buf[..n]),
                        }
                    }
                    for (pause, bytes) in steps {
                        tokio::time::sleep(pause).await;
                        if stream.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        url
    }

    const PUT_ANSWER: &[u8] = br#"{"path":"/f","bytes_written":131072}"#;

    fn answer_with_length(body: &[u8]) -> Vec<u8> {
        let mut out =
            format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn a_transfer_gets_a_second_for_every_64_kib() {
        assert_eq!(transfer_time(0), Duration::ZERO);
        assert_eq!(transfer_time(32 * 1024), Duration::from_millis(500));
        assert_eq!(transfer_time(10 * 1024 * 1024), Duration::from_secs(160));
    }

    #[tokio::test]
    async fn a_large_upload_outlives_a_short_client_bound() {
        // 128 KiB gets two more seconds; the answer comes after 600 ms.
        let url = scripted_server(vec![(
            Duration::from_millis(600),
            answer_with_length(PUT_ANSWER),
        )])
        .await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(200));
        let put = client.files_put_raw("/f", vec![7; 128 * 1024]).await;
        assert!(put.is_ok(), "{put:?}");
        // A small upload has only the client's bound.
        let err = client.files_put_raw("/f", vec![7; 16]).await.unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
    }

    #[tokio::test]
    async fn a_large_download_gets_time_for_its_length() {
        let body = vec![b'x'; 128 * 1024];
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
        let url = scripted_server(vec![
            (Duration::ZERO, head.into_bytes()),
            (Duration::from_millis(600), body.clone()),
        ])
        .await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(200));
        assert_eq!(client.files_get_raw("/f").await.unwrap(), body);
    }

    #[tokio::test]
    async fn a_download_of_unknown_length_is_bounded_by_idle_time() {
        let head = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
        let pace = |pause: u64| {
            let mut steps = vec![(Duration::ZERO, head.clone())];
            for _ in 0..5 {
                steps.push((Duration::from_millis(pause), b"abc".to_vec()));
            }
            steps
        };
        // Five pieces 120 ms apart: 600 ms in all, past the 300 ms bound, but never idle that long.
        let url = scripted_server(pace(120)).await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(300));
        assert_eq!(client.files_get_raw("/f").await.unwrap(), b"abc".repeat(5));
        // A piece that is late by more than the bound is a timeout.
        let url = scripted_server(pace(600)).await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(300));
        let err = client.files_get_raw("/f").await.unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
    }

    #[test]
    fn browser_waits_and_runs_get_their_own_time() {
        let client = BuwizComputer::connect("http://127.0.0.1:1", "http://127.0.0.1:2", "t")
            .unwrap()
            .with_timeout(Duration::from_secs(1));
        assert_eq!(
            client.browser_bound(&json!({ "timeout_ms": 60_000 }), DEFAULT_BROWSER_WAIT),
            Duration::from_secs(80)
        );
        assert_eq!(
            client.browser_bound(&json!({}), DEFAULT_BROWSER_RUN),
            Duration::from_secs(140)
        );
        assert_eq!(
            client.on(":3").browser("snap"),
            "http://127.0.0.1:1/v1/browser/snap?display=%3A3"
        );
    }

    #[tokio::test]
    async fn a_tree_read_outlives_a_short_client_bound() {
        let url = slow_server(Duration::from_millis(400)).await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(100));
        assert_eq!(
            client.tree_bound(&json!({ "timeout_ms": 10_000 })),
            Duration::from_secs(15)
        );
        assert_eq!(
            client.cua_tree(json!({ "timeout_ms": 100 })).await.unwrap(),
            json!({})
        );
    }

    #[test]
    fn the_default_bound_is_two_minutes() {
        let client =
            BuwizComputer::connect("http://127.0.0.1:1337", "http://127.0.0.1:1340", "tok")
                .unwrap();
        assert_eq!(client.timeout(), Duration::from_secs(120));
        let client = client.with_timeout(Duration::from_secs(5));
        assert_eq!(client.timeout(), Duration::from_secs(5));
    }

    #[test]
    fn an_exec_gets_at_least_its_own_timeout() {
        let client =
            BuwizComputer::connect("http://127.0.0.1:1337", "http://127.0.0.1:1340", "tok")
                .unwrap();
        assert_eq!(client.exec_bound(&exec_request(None)), DEFAULT_TIMEOUT);
        assert_eq!(
            client.exec_bound(&exec_request(Some(1_000))),
            DEFAULT_TIMEOUT
        );
        assert_eq!(
            client.exec_bound(&exec_request(Some(600_000))),
            Duration::from_secs(600) + EXEC_MARGIN
        );
    }

    #[tokio::test]
    async fn a_silent_server_is_a_timeout() {
        let url = slow_server(Duration::from_secs(30)).await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let err = client.health_exec().await.unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_slow_answer_inside_the_bound_arrives() {
        let url = slow_server(Duration::from_millis(100)).await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_secs(5));
        assert_eq!(client.health_exec().await.unwrap(), json!({}));
    }

    #[tokio::test]
    async fn a_screen_wait_outlives_a_short_client_bound() {
        let url = slow_server(Duration::from_millis(400)).await;
        let client = BuwizComputer::connect(&url, &url, "tok")
            .unwrap()
            .with_timeout(Duration::from_millis(100));
        // The wait's own 100 ms plus the margin, not the client's 100 ms.
        assert_eq!(
            client.cua_wait(json!({ "timeout": 100 })).await.unwrap(),
            json!({})
        );
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
