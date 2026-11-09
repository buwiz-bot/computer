use crate::{app, AppState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

fn state(dir: &TempDir) -> AppState {
    AppState::for_test(dir.path().canonicalize().unwrap(), "secret-token")
}

async fn send(state: AppState, builder: Request<Body>) -> (StatusCode, Value) {
    let app = app(state);
    let response = app.oneshot(builder).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}));
    (status, json)
}

fn auth_json(method: &str, uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn health_is_public() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .uri("/v1/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(body.get("service").is_none());
    assert!(body.get("version").is_none());
}

#[tokio::test]
async fn exec_rejects_missing_auth() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("POST")
            .uri("/v1/exec")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"command":["echo","ok"]}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn exec_rejects_wrong_token() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "wrong",
            json!({"command":["echo","ok"]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn exec_echo_ok() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({"command":["echo","ok"]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["timed_out"], false);
    assert!(body["stdout"].as_str().unwrap().contains("ok"));
}

#[tokio::test]
async fn files_reject_path_escape() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("GET")
            .uri("/v1/files?path=../etc/passwd")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "path_escape");
}

#[tokio::test]
async fn files_put_and_get() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let (status, body) = send(
        s.clone(),
        auth_json(
            "PUT",
            "/v1/files",
            "secret-token",
            json!({"path":"notes/hello.txt","content":"hello from buwiz-computer"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bytes_written"], "hello from buwiz-computer".len());

    let (status, body) = send(
        s,
        Request::builder()
            .method("GET")
            .uri("/v1/files?path=notes/hello.txt")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "file");
    assert_eq!(body["content"], "hello from buwiz-computer");
}

#[tokio::test]
async fn files_put_rejects_absolute_escape() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "PUT",
            "/v1/files",
            "secret-token",
            json!({"path":"/etc/passwd","content":"nope"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "path_escape");
}

#[tokio::test]
async fn cua_rejects_missing_auth() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("POST")
            .uri("/v1/cua/screenshot")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn cua_screenshot_disabled() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("POST")
            .uri("/v1/cua/screenshot")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "cua_disabled");
}

#[tokio::test]
async fn cua_click_disabled() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/cua/click",
            "secret-token",
            json!({"x": 10, "y": 10}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "cua_disabled");
}

#[tokio::test]
async fn cua_type_key_scroll_disabled() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    for (uri, payload) in [
        ("/v1/cua/type", json!({"text": "hi"})),
        ("/v1/cua/key", json!({"key": "Return"})),
        (
            "/v1/cua/scroll",
            json!({"x": 10, "y": 10, "dx": 0, "dy": 1}),
        ),
        ("/v1/cua/double-click", json!({"x": 10, "y": 10})),
        ("/v1/cua/move", json!({"x": 10, "y": 10})),
        (
            "/v1/cua/drag",
            json!({"x1": 10, "y1": 10, "x2": 20, "y2": 20}),
        ),
        ("/v1/cua/press", json!({"x": 10, "y": 10, "button": 1})),
        ("/v1/cua/mousedown", json!({"x": 10, "y": 10, "button": 1})),
        ("/v1/cua/release", json!({"x": 20, "y": 20, "button": 1})),
        ("/v1/cua/mouseup", json!({"x": 20, "y": 20, "button": 1})),
        (
            "/v1/cua/recipe",
            json!({"steps":[{"op":"move","x":10,"y":10}]}),
        ),
        ("/v1/cua/wait", json!({"change": true})),
        ("/v1/cua/record/start", json!({})),
        ("/v1/cua/record/stop", json!({"id": 1})),
    ] {
        let (status, body) = send(s.clone(), auth_json("POST", uri, "secret-token", payload)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(body["error"]["code"], "cua_disabled", "{uri}");
    }
}

#[tokio::test]
async fn cua_screenshot_png_query_still_json_error_when_disabled() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("POST")
            .uri("/v1/cua/screenshot?format=png")
            .header("authorization", "Bearer secret-token")
            .header("accept", "image/png")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "cua_disabled");
}

#[tokio::test]
async fn files_mkdir_and_delete() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let (status, body) = send(
        s.clone(),
        auth_json(
            "POST",
            "/v1/files/mkdir",
            "secret-token",
            json!({"path": "notes/sub", "parents": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["created"], true);

    let (status, body) = send(
        s.clone(),
        auth_json(
            "PUT",
            "/v1/files",
            "secret-token",
            json!({"path": "notes/sub/a.txt", "content": "x"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bytes_written"], 1);

    let (status, body) = send(
        s.clone(),
        Request::builder()
            .method("DELETE")
            .uri("/v1/files?path=notes/sub/a.txt")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], true);

    let (status, body) = send(
        s,
        Request::builder()
            .method("DELETE")
            .uri("/v1/files?path=notes/sub&recursive=true")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], true);
}

#[tokio::test]
async fn files_rename() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let (status, _) = send(
        s.clone(),
        auth_json(
            "PUT",
            "/v1/files",
            "secret-token",
            json!({"path": "old.txt", "content": "x"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        s.clone(),
        auth_json(
            "POST",
            "/v1/files/rename",
            "secret-token",
            json!({"from": "old.txt", "to": "new.txt"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["to"].as_str().unwrap().ends_with("new.txt"));

    let (status, body) = send(
        s,
        Request::builder()
            .method("GET")
            .uri("/v1/files?path=new.txt")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "x");
}

#[tokio::test]
async fn files_delete_rejects_root() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("DELETE")
            .uri("/v1/files?path=")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn exec_strips_box_token_from_child_env() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": ["sh", "-c", r#"if [ -n "${BOX_TOKEN+x}" ]; then echo LEAKED; else echo STRIPPED; fi"#],
                "env": {"BOX_TOKEN": "injected-secret"}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exit_code"], 0);
    assert!(body["stdout"].as_str().unwrap().contains("STRIPPED"));
    assert!(!body["stdout"].as_str().unwrap().contains("LEAKED"));
}

#[tokio::test]
async fn exec_sets_default_term() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": ["sh", "-c", "printf '%s %s %s' \"$TERM\" \"$COLUMNS\" \"$LINES\""]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout"], "xterm-256color 120 32");
}

#[tokio::test]
async fn exec_env_can_override_term() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": ["sh", "-c", "printf '%s' \"$TERM\""],
                "env": {"TERM": "dumb"}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stdout"], "dumb");
}

#[tokio::test]
async fn cua_recipe_rejects_empty_before_display() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/cua/recipe",
            "secret-token",
            json!({"steps": []}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn cua_recipe_rejects_out_of_range_before_run() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/cua/recipe",
            "secret-token",
            json!({
                "screenshot": "none",
                "steps": [
                    {"op": "click", "x": 10, "y": 10},
                    {"op": "click", "x": 1280, "y": 0}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "out_of_range");
}

#[tokio::test]
async fn exec_closes_stdin_so_cat_exits() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": ["cat"],
                "stdin": "from-stdin\n",
                "timeout_ms": 2000
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["timed_out"], false);
    assert_eq!(body["exit_code"], 0);
    assert!(body["stdout"].as_str().unwrap().contains("from-stdin"));
}

#[tokio::test]
async fn exec_timeout_keeps_stdout() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": ["sh", "-c", "echo hello >.timeout-out; cat .timeout-out; sleep 30"],
                "timeout_ms": 500
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["timed_out"], true);
    assert!(
        body["exec_id"].as_str().unwrap().starts_with("exec-"),
        "missing exec_id: {body}"
    );
    assert!(
        body["stdout"].as_str().unwrap().contains("hello"),
        "timeout discarded stdout: {body}"
    );
}

#[tokio::test]
async fn request_id_is_echoed() {
    let dir = TempDir::new().unwrap();
    let app = app(state(&dir));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .header("x-request-id", "smoke-id-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "smoke-id-1"
    );
}

#[tokio::test]
async fn busy_reports_slots() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .uri("/v1/busy")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["execs"]["max"], 8);
    assert_eq!(body["busy"], false);
}

#[tokio::test]
async fn pty_is_rejected() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({"command":["echo","ok"],"pty":true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn files_raw_roundtrip() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let put = app(s.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/files/raw?path=bin.dat")
                .header("authorization", "Bearer secret-token")
                .header("content-type", "application/octet-stream")
                .body(Body::from(vec![0u8, 1, 2, 255]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);

    let get = app(s)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/files/raw?path=bin.dat")
                .header("authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(
        get.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
    let bytes = get.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], &[0u8, 1, 2, 255]);
}

#[tokio::test]
async fn exec_stream_ndjson_exits() {
    let dir = TempDir::new().unwrap();
    let app = app(state(&dir));
    let response = app
        .oneshot(auth_json(
            "POST",
            "/v1/exec/stream",
            "secret-token",
            json!({"command":["echo","stream-ok"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ctype = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ctype.contains("ndjson"), "{ctype}");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("stream-ok"), "{text}");
    assert!(
        text.contains("\"type\":\"exit\"") || text.contains("\"type\": \"exit\""),
        "{text}"
    );
}

#[tokio::test]
async fn exec_busy_returns_429() {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    let dir = TempDir::new().unwrap();
    let mut s = state(&dir);
    s.max_concurrent_execs = 1;
    s.exec_slots = Arc::new(Semaphore::new(1));
    let app_a = app(s.clone());
    let app_b = app(s);
    let first = tokio::spawn(async move {
        app_a
            .oneshot(auth_json(
                "POST",
                "/v1/exec",
                "secret-token",
                json!({"command":["sleep","2"],"timeout_ms":5000}),
            ))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let response = app_b
        .oneshot(auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({"command":["echo","x"]}),
        ))
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"raw": ""}));
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "busy");
    let _ = first.await;
}

#[tokio::test]
async fn exec_detach_then_status() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let (status, body) = send(
        s.clone(),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({"command":["echo","detached-ok"],"detach":true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["detached"], true);
    assert_eq!(body["status"], "running");
    let id = body["exec_id"].as_str().expect("exec_id").to_string();
    let mut last = body;
    for _ in 0..80 {
        let (st, b) = send(
            s.clone(),
            Request::builder()
                .uri(format!("/v1/exec/{id}"))
                .header("authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        last = b;
        if last["status"] == "done" || last["exit_code"] == 0 {
            let stdout = last["stdout"].as_str().unwrap_or("");
            assert!(stdout.contains("detached-ok"), "{last}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("detached job never finished: {last}");
}

/// Argv that prints a marker, leaves a grandchild holding both pipes, and then
/// blocks. `exec` keeps the grandchild's pid equal to the one it reports, and a
/// non-interactive `sh` puts background jobs in its own process group, so the
/// grandchild is exactly what the exec's group signal has to reach.
fn spawns_a_grandchild(pid_file: &str) -> String {
    format!("sh -c 'echo $$ > {pid_file}; exec sleep 60' & echo spawned; sleep 60")
}

/// Poll until the pid file exists and parses. The shell writes it a moment
/// after the request starts, so "it is there" is not something the caller can
/// assume from having sent the request.
async fn wait_for_pid(path: &std::path::Path) -> i32 {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                if pid > 0 {
                    return pid;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("grandchild never reported its pid: {}", path.display());
}

/// `kill(pid, 0)` succeeds for a live process and for a zombie; an orphan is
/// reparented to init and reaped, so the signal starting to fail is the
/// observable "this process tree is gone".
async fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..200 {
        // Safety: signal 0 performs the permission and existence check only.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn exec_keeps_foreground_output_when_a_background_process_survives() {
    let dir = TempDir::new().unwrap();
    let started = std::time::Instant::now();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": "echo BEFORE; (sleep 5 &); echo AFTER",
                "timeout_ms": 5000
            }),
        ),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    let stdout = body["stdout"].as_str().unwrap_or("");
    assert!(
        stdout.contains("BEFORE"),
        "dropped output before the fork: {body}"
    );
    assert!(
        stdout.contains("AFTER"),
        "dropped output after the fork: {body}"
    );
    assert_eq!(body["exit_code"], 0, "{body}");
    // The grandchild still holds both write ends, so the daemon cannot promise
    // it saw everything and must not claim it did by omission.
    assert_eq!(body["output_complete"], false, "{body}");
    assert_eq!(body["truncated"], true, "{body}");
    assert!(
        elapsed < Duration::from_secs(1),
        "waiting for an EOF that is not coming: {elapsed:?}"
    );
}

#[tokio::test]
async fn exec_reports_complete_output_for_an_ordinary_command() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({"command": ["echo", "plain"]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["output_complete"], true, "{body}");
    assert_eq!(body["truncated"], false, "{body}");
}

#[tokio::test]
async fn exec_with_background_process_does_not_leak_descriptors() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let body = json!({"command": "(sleep 5 &); echo hi", "timeout_ms": 5000});
    // The first calls settle descriptors tokio opens lazily (signal driver and
    // friends), so the measurement below is only about the exec path.
    for _ in 0..3 {
        let (status, _) = send(
            s.clone(),
            auth_json("POST", "/v1/exec", "secret-token", body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let before = crate::fdcount::open_fd_count().expect("this platform reports open descriptors");
    for _ in 0..10 {
        let (status, _) = send(
            s.clone(),
            auth_json("POST", "/v1/exec", "secret-token", body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let after = crate::fdcount::open_fd_count().expect("this platform reports open descriptors");
    assert!(
        after <= before + 2,
        "each background-spawning exec leaked descriptors: {before} -> {after}"
    );
}

#[tokio::test]
async fn cancelled_request_kills_the_whole_process_group() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let call = app(state(&dir)).oneshot(auth_json(
        "POST",
        "/v1/exec",
        "secret-token",
        json!({
            "command": ["sh", "-c", spawns_a_grandchild("grandchild.pid")],
            "timeout_ms": 10000
        }),
    ));
    // Run it on its own task and abort: aborting drops the handler future,
    // which is exactly what axum does to it when the HTTP client disconnects.
    let inflight = tokio::spawn(call);
    let pid = wait_for_pid(&pid_file).await;
    inflight.abort();
    assert!(
        wait_until_gone(pid).await,
        "grandchild {pid} outlived the cancelled request"
    );
}

#[tokio::test]
async fn stream_client_disconnect_kills_the_whole_process_group() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let response = app(state(&dir))
        .oneshot(auth_json(
            "POST",
            "/v1/exec/stream",
            "secret-token",
            json!({
                "command": ["sh", "-c", spawns_a_grandchild("grandchild.pid")],
                "timeout_ms": 10000
            }),
        ))
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let pid = wait_for_pid(&pid_file).await;
    // Dropping the response drops the body stream, and with it the receiver
    // the exec task is writing into — the streaming equivalent of hanging up.
    drop(response);
    assert!(
        wait_until_gone(pid).await,
        "grandchild {pid} outlived the disconnected stream"
    );
}

#[tokio::test]
async fn cancel_endpoint_kills_a_detached_process_group() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    let pid_file = dir.path().join("grandchild.pid");
    let (status, body) = send(
        s.clone(),
        auth_json(
            "POST",
            "/v1/exec",
            "secret-token",
            json!({
                "command": ["sh", "-c", spawns_a_grandchild("grandchild.pid")],
                "detach": true,
                "timeout_ms": 10000
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["exec_id"].as_str().expect("exec_id").to_string();
    let pid = wait_for_pid(&pid_file).await;

    let (status, body) = send(
        s.clone(),
        Request::builder()
            .method("DELETE")
            .uri(format!("/v1/exec/{id}"))
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cancelled"], true, "{body}");
    assert!(
        wait_until_gone(pid).await,
        "grandchild {pid} outlived DELETE /v1/exec/{id}"
    );
}

#[tokio::test]
async fn cancel_rejects_an_unknown_exec_id() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .method("DELETE")
            .uri("/v1/exec/exec-does-not-exist")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn metrics_reports_child_groups_and_open_descriptors() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        state(&dir),
        Request::builder()
            .uri("/v1/metrics")
            .header("authorization", "Bearer secret-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["execs"]["child_groups"], 0, "{body}");
    assert!(
        body["open_fds"].as_u64().unwrap_or(0) > 0,
        "open descriptor gauge missing: {body}"
    );
}

#[tokio::test]
async fn tree_and_element_take_the_token() {
    let dir = TempDir::new().unwrap();
    for uri in ["/v1/cua/tree", "/v1/cua/element"] {
        let (status, body) = send(
            state(&dir),
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ref":"1:0","action":"press"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(body["error"]["code"], "unauthorized", "{uri}");
    }
}

#[tokio::test]
async fn tree_and_element_are_cua_routes() {
    let dir = TempDir::new().unwrap();
    let s = state(&dir);
    for (uri, payload) in [
        ("/v1/cua/tree", json!({})),
        ("/v1/cua/element", json!({"ref": "1:0", "action": "press"})),
    ] {
        let (status, body) = send(s.clone(), auth_json("POST", uri, "secret-token", payload)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(body["error"]["code"], "cua_disabled", "{uri}");
    }
    let (status, body) = send(
        s,
        auth_json(
            "POST",
            "/v1/cua/tree?display=nope",
            "secret-token",
            json!({}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

/// CUA on, and no accessibility bus: what a box without one, or this machine, has.
fn a11y_state(dir: &TempDir) -> AppState {
    let mut s = state(dir);
    s.cua.enabled = true;
    s
}

#[tokio::test]
async fn tree_without_a_bus_says_so() {
    let dir = TempDir::new().unwrap();
    let (status, body) = send(
        a11y_state(&dir),
        auth_json(
            "POST",
            "/v1/cua/tree?display=:97",
            "secret-token",
            json!({"scope": "desktop"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "a11y_unavailable");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains(":97 has no accessibility bus"));
}

#[tokio::test]
async fn tree_limits_are_checked_first() {
    let dir = TempDir::new().unwrap();
    for payload in [
        json!({"max_nodes": 0}),
        json!({"max_nodes": 1_000_000}),
        json!({"max_depth": 1000}),
        json!({"timeout_ms": 600_000}),
    ] {
        let (status, body) = send(
            a11y_state(&dir),
            auth_json("POST", "/v1/cua/tree", "secret-token", payload.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{payload}");
        assert_eq!(body["error"]["code"], "invalid_request", "{payload}");
    }
}

#[tokio::test]
async fn element_refs_are_checked_before_the_bus() {
    let dir = TempDir::new().unwrap();
    let s = a11y_state(&dir);
    let cases = [
        (
            json!({"ref": "nope", "action": "press"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json!({"ref": "5:1", "action": "set_text"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json!({"ref": "5:1", "action": "press"}),
            StatusCode::CONFLICT,
            "stale_ref",
        ),
    ];
    for (payload, want, code) in cases {
        let (status, body) = send(
            s.clone(),
            auth_json("POST", "/v1/cua/element", "secret-token", payload.clone()),
        )
        .await;
        assert_eq!(status, want, "{payload}: {body}");
        assert_eq!(body["error"]["code"], code, "{payload}");
    }
}

mod browser_routes {
    use super::*;
    use box_browser::seam::{GoalBounds, GoalReport, PolicyLists, ToolFailure};
    use box_browser::{BrowserError, Browsers, Opener, Rules, Tools};
    use std::sync::Arc;

    const ROUTES: [&str; 8] = [
        "/v1/browser/snap",
        "/v1/browser/act",
        "/v1/browser/navigate",
        "/v1/browser/tabs",
        "/v1/browser/wait",
        "/v1/browser/shot",
        "/v1/browser/run",
        "/v1/browser/state",
    ];

    /// A page with a search button and a pay button, as ultra-instinct would read it.
    struct Shop;
    impl Tools for Shop {
        fn call(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
            Ok(match name {
                "browser_get_state" => json!({"url": "https://shop.example/", "title": "Shop",
                    "interactive_elements": 2, "elements": [
                    {"index": 1, "region": "n1", "role": "button", "label": "Search",
                     "actions": ["click"], "rect": {"x": 0, "y": 0, "w": 10, "h": 10}},
                    {"index": 2, "region": "n2", "role": "button", "label": "Pay $18",
                     "actions": ["click"], "rect": {"x": 0, "y": 20, "w": 10, "h": 10}}]}),
                "browser_click" => {
                    json!({"acted": "click", "index": args["index"], "region": format!("n{}", args["index"]), "ticket": 3})
                }
                other => return Err(ToolFailure::Invalid(format!("not faked: {other}"))),
            })
        }
        fn set_policy(&mut self, _policy: &PolicyLists) -> Result<(), ToolFailure> {
            Ok(())
        }
        fn run_goal(&mut self, _goal: &str, _b: &GoalBounds) -> Result<GoalReport, ToolFailure> {
            Err(ToolFailure::Unavailable("not faked".into()))
        }
        fn tabs(&mut self) -> Result<Vec<box_browser::seam::TabInfo>, ToolFailure> {
            Ok(Vec::new())
        }
        fn open_tab(&mut self, _url: &str) -> Result<String, ToolFailure> {
            Ok("T1".into())
        }
        fn switch_tab(&mut self, _id: &str) -> Result<(), ToolFailure> {
            Ok(())
        }
        fn close_tab(&mut self, _id: &str) -> Result<(), ToolFailure> {
            Ok(())
        }
    }
    struct ShopOpener;
    impl Opener for ShopOpener {
        fn open(&self, _display: &str) -> Result<Box<dyn Tools>, BrowserError> {
            Ok(Box::new(Shop))
        }
    }

    fn with(dir: &TempDir, opener: Arc<dyn Opener>) -> AppState {
        let mut s = state(dir);
        s.browser = Browsers::new(true, Rules::default(), opener, dir.path().join("state"));
        s
    }

    #[tokio::test]
    async fn browser_routes_take_the_token() {
        let dir = TempDir::new().unwrap();
        for uri in ROUTES {
            let (status, body) =
                send(state(&dir), auth_json("POST", uri, "wrong", json!({}))).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
            assert_eq!(body["error"]["code"], "unauthorized");
        }
    }

    #[tokio::test]
    async fn an_off_browser_layer_says_so() {
        let dir = TempDir::new().unwrap();
        let (status, body) = send(
            state(&dir),
            auth_json("POST", "/v1/browser/snap", "secret-token", json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "browser_disabled");
    }

    #[tokio::test]
    async fn a_screen_whose_launcher_offers_no_pipe_is_unavailable() {
        let dir = TempDir::new().unwrap();
        let s = with(&dir, Arc::new(box_browser::pipe::PipeOpener));
        let (status, body) = send(
            s,
            auth_json("POST", "/v1/browser/snap", "secret-token", json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "browser_unavailable");
    }

    #[tokio::test]
    async fn a_goal_run_is_503_until_ultra_instinct_has_one() {
        let dir = TempDir::new().unwrap();
        let s = with(&dir, Arc::new(ShopOpener));
        let (status, body) = send(
            s,
            auth_json(
                "POST",
                "/v1/browser/run",
                "secret-token",
                json!({"goal": "Click Search"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "browser_unavailable");
    }

    #[tokio::test]
    async fn the_display_and_the_limits_are_checked() {
        let dir = TempDir::new().unwrap();
        let s = with(&dir, Arc::new(ShopOpener));
        let (status, _) = send(
            s.clone(),
            auth_json(
                "POST",
                "/v1/browser/snap?display=nope",
                "secret-token",
                json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = send(
            s.clone(),
            auth_json(
                "POST",
                "/v1/browser/snap",
                "secret-token",
                json!({"max_nodes": 0}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
        let (status, body) = send(
            s,
            auth_json(
                "POST",
                "/v1/browser/run",
                "secret-token",
                json!({"goal": "x", "max_steps": 500}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn a_pay_click_is_409_confirm_required_until_its_token_comes_back() {
        let dir = TempDir::new().unwrap();
        let s = with(&dir, Arc::new(ShopOpener));
        let (status, body) = send(
            s.clone(),
            auth_json(
                "POST",
                "/v1/browser/act",
                "secret-token",
                json!({"ref": "@e2", "action": "click"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "stale_ref");

        let (status, snap) = send(
            s.clone(),
            auth_json(
                "POST",
                "/v1/browser/snap?display=:1",
                "secret-token",
                json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{snap}");
        assert_eq!(snap["elements"][1]["ref"], "@e2");
        assert_eq!(snap["elements"][1]["label"], "Pay $18");

        let click = |confirm: Option<&str>| {
            let mut body = json!({"ref": "@e2", "action": "click", "snapshot": snap["snapshot"]});
            if let Some(token) = confirm {
                body["confirm"] = json!(token);
            }
            auth_json("POST", "/v1/browser/act", "secret-token", body)
        };
        let (status, body) = send(s.clone(), click(None)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "confirm_required");
        assert_eq!(body["error"]["details"]["label"], "Pay $18");
        let token = body["error"]["details"]["confirm"]
            .as_str()
            .unwrap()
            .to_string();

        let (status, body) = send(s.clone(), click(Some(&token))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["ticket"], 3);
    }

    #[tokio::test]
    async fn off_the_allow_list_is_403() {
        let dir = TempDir::new().unwrap();
        let s = with(&dir, Arc::new(ShopOpener));
        let (status, body) = send(
            s,
            auth_json(
                "POST",
                "/v1/browser/navigate",
                "secret-token",
                json!({"url": "https://evil.example/", "allow_domains": ["shop.example"]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "domain_not_allowed");
        assert_eq!(body["error"]["details"]["host"], "evil.example");
    }
}
