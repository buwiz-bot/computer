use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use box_common::{
    new_prefixed_id, resolve_in_canonical_jail, ApiError, SECRET_ENV_KEYS, SECRET_FILE_ENV_KEYS,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{AppState, ChildGroups, DetachedJob};

const MAX_DETACHED_JOBS: usize = 64;

/// How long to keep reading a child's pipes after the child itself has exited.
///
/// A command that leaves something running in the background hands that
/// process the write ends of both pipes, so EOF never arrives and "pipe
/// closed" is not a usable completion condition. Everything the foreground
/// wrote is already sitting in the pipe buffer by the time the child exits, so
/// a short window collects it; waiting longer only charges the caller for an
/// EOF that is not coming.
const POST_EXIT_DRAIN: Duration = Duration::from_millis(50);

/// Sensible tty-ish defaults so `clear`, `tput`, and color-aware tools work
/// even though exec is a pipe, not a PTY. Callers can override via `env`.
const DEFAULT_CHILD_ENV: &[(&str, &str)] = &[
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("COLUMNS", "120"),
    ("LINES", "32"),
];

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    /// Argv (`["echo","ok"]`) or a shell string (`"echo ok"` → `/bin/sh -c`).
    pub command: CommandSpec,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    pub env: Option<HashMap<String, String>>,
    pub stdin: Option<String>,
    /// Deferred. PTY exec is not implemented; use `/v1/exec/stream`.
    pub pty: Option<bool>,
    /// Return immediately; poll `GET /v1/exec/{id}`.
    pub detach: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    Argv(Vec<String>),
    Shell(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The returned output is not everything the command produced: either the
    /// `BOX_MAX_OUTPUT_BYTES` cap was hit, or reading stopped before EOF
    /// (`output_complete: false`).
    pub truncated: bool,
    /// Both pipes reached EOF before this response was built. `false` means
    /// something the command left running still holds them, so anything
    /// written after the foreground exited was not captured — run it with
    /// `detach: true` or `POST /v1/exec/stream` if you need that output.
    pub output_complete: bool,
    pub cwd: String,
    pub exec_id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub exec_id: String,
    /// `false` when the id is known but the exec had already finished.
    pub cancelled: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    Stdout {
        data: String,
    },
    Stderr {
        data: String,
    },
    Exit {
        exec_id: String,
        exit_code: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
        truncated: bool,
        output_complete: bool,
        cwd: String,
    },
}

struct Prepared {
    argv: Vec<String>,
    cwd: PathBuf,
    timeout: Duration,
    env: Option<HashMap<String, String>>,
    stdin: Option<String>,
    exec_id: String,
}

struct SlotGuard {
    _permit: tokio::sync::OwnedSemaphorePermit,
    inflight: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Owns a running child together with the id of the process group it leads.
///
/// Every way out of an exec — the command finishing, the timeout firing, or
/// the caller's future being dropped because the HTTP client went away — has
/// to converge on one cleanup path. Tokio's `kill_on_drop` only reaches the
/// group leader, so a disconnect used to leave the whole descendant tree
/// running with nothing left in the process that still knew its group id.
struct ChildGuard {
    child: Option<tokio::process::Child>,
    /// Captured at spawn. `Child::id()` returns `None` once the child has been
    /// waited on, which is exactly when the cleanup paths need it.
    pgid: Option<i32>,
    exec_id: String,
    kill_grace: Duration,
    groups: ChildGroups,
}

impl ChildGuard {
    fn adopt(child: tokio::process::Child, exec_id: &str, state: &AppState) -> Self {
        // `spawn_child` calls `process_group(0)`, so the child's pid is also
        // the id of the group it leads.
        let pgid = child.id().map(|pid| pid as i32);
        if let Some(pgid) = pgid {
            lock_groups(&state.child_groups).insert(exec_id.to_string(), pgid);
        }
        Self {
            child: Some(child),
            pgid,
            exec_id: exec_id.to_string(),
            kill_grace: state.kill_grace,
            groups: state.child_groups.clone(),
        }
    }

    /// Stop advertising this group and hand the child back.
    ///
    /// Nothing may reap the child before this has returned: `DELETE
    /// /v1/exec/{id}` signals the group while holding the same lock, so
    /// dropping the table entry first is what guarantees a cancel can never
    /// land on a pid the kernel has already recycled.
    fn deregister(&mut self) -> Option<tokio::process::Child> {
        if self.pgid.is_some() {
            lock_groups(&self.groups).remove(&self.exec_id);
        }
        self.child.take()
    }

    /// Wait for the child, escalating to the whole process group on timeout.
    async fn wait(&mut self, timeout: Duration) -> Result<(bool, Option<i32>), ApiError> {
        let first = {
            let child = self
                .child
                .as_mut()
                .ok_or_else(|| ApiError::internal("exec child already reaped"))?;
            tokio::time::timeout(timeout, child.wait()).await
        };
        match first {
            Ok(Ok(status)) => {
                let code = status.code();
                self.deregister();
                return Ok((false, code));
            }
            Ok(Err(err)) => return Err(ApiError::io(err.to_string())),
            Err(_) => {}
        }

        signal_group(self.pgid, term_signal());
        let graceful = {
            let child = self
                .child
                .as_mut()
                .ok_or_else(|| ApiError::internal("exec child already reaped"))?;
            tokio::time::timeout(self.kill_grace, child.wait()).await
        };
        match graceful {
            Ok(Ok(status)) => {
                let code = status.code();
                self.deregister();
                Ok((true, code))
            }
            Ok(Err(err)) => Err(ApiError::io(err.to_string())),
            Err(_) => {
                signal_group(self.pgid, kill_signal());
                let Some(mut child) = self.deregister() else {
                    return Ok((true, None));
                };
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok((true, None))
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let pgid = self.pgid;
        let grace = self.kill_grace;
        let Some(mut child) = self.deregister() else {
            return;
        };
        // Only reached when the exec did not finish on its own: the request
        // future was dropped (client disconnect, or the stream's receiver went
        // away) or the handler bailed out. Signal the group, because
        // `kill_on_drop` would SIGKILL the direct child alone and leave every
        // descendant running and unreachable.
        signal_group(pgid, term_signal());
        #[cfg(not(unix))]
        let _ = child.start_kill();
        // `Drop` cannot await, so the grace period, the escalation to SIGKILL
        // and the reap move to a detached task. Holding the unreaped child
        // until that task runs is what keeps `pgid` from naming a pid the
        // kernel has handed to somebody else in the meantime.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    tokio::time::sleep(grace).await;
                    signal_group(pgid, kill_signal());
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                });
            }
            Err(_) => {
                // No runtime left to escalate on (the process is shutting
                // down). Skip the grace period rather than leave the group.
                signal_group(pgid, kill_signal());
                let _ = child.start_kill();
            }
        }
    }
}

fn lock_groups(groups: &ChildGroups) -> std::sync::MutexGuard<'_, HashMap<String, i32>> {
    // A panic while the table is locked must not take out every later exec:
    // the map is a plain id → pgid index with no invariant to corrupt.
    groups.lock().unwrap_or_else(|err| err.into_inner())
}

pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, ApiError> {
    reject_pty(&req)?;
    let detach = req.detach.unwrap_or(false);
    let prepared = prepare(&state, req)?;
    if detach {
        return detach_run(state, prepared).await;
    }
    let _slot = admit(&state)?;
    Ok(Json(run_blocking(&state, prepared).await?))
}

pub async fn handle_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExecRequest>,
) -> Result<Response, ApiError> {
    reject_pty(&req)?;
    let sse = wants_sse(&headers);
    let prepared = prepare(&state, req)?;
    let slot = admit(&state)?;
    let (tx, rx) = mpsc::channel::<StreamEvent>(64);
    let exec_id = prepared.exec_id.clone();
    let cwd_display = prepared.cwd.display().to_string();
    let child_state = state.clone();

    tokio::spawn(async move {
        let _slot = slot;
        if let Err(err) = stream_child(prepared, child_state, &tx).await {
            let _ = tx
                .send(StreamEvent::Stderr {
                    data: format!("exec failed: {err}"),
                })
                .await;
            let _ = tx
                .send(StreamEvent::Exit {
                    exec_id,
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    truncated: false,
                    output_complete: false,
                    cwd: cwd_display,
                })
                .await;
        }
    });

    let stream = ReceiverStream::new(rx).map(move |evt| {
        let payload = serde_json::to_vec(&evt).unwrap_or_else(|_| b"{}".to_vec());
        let bytes = if sse {
            let mut v = b"data: ".to_vec();
            v.extend_from_slice(&payload);
            v.extend_from_slice(b"\n\n");
            Bytes::from(v)
        } else {
            let mut v = payload;
            v.push(b'\n');
            Bytes::from(v)
        };
        Ok::<_, std::io::Error>(bytes)
    });
    let ctype = if sse {
        "text/event-stream"
    } else {
        "application/x-ndjson"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|err| ApiError::internal(err.to_string()))
}

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ExecResponse>, ApiError> {
    let jobs = state.jobs.lock().await;
    let job = jobs
        .get(&id)
        .ok_or_else(|| ApiError::not_found("exec id not found"))?;
    if let Some(result) = job.result.clone() {
        return Ok(Json(result));
    }
    Ok(Json(ExecResponse {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        truncated: false,
        output_complete: false,
        cwd: String::new(),
        exec_id: id,
        detached: true,
        status: Some(job.status),
    }))
}

/// `DELETE /v1/exec/{id}` — stop a running exec and its whole process group.
///
/// Without this, cancellation was only expressible as "drop the TCP
/// connection", which is the one action that used to strand the process tree.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CancelResponse>, ApiError> {
    if cancel_group(&state, &id) {
        return Ok(Json(CancelResponse {
            exec_id: id,
            cancelled: true,
        }));
    }
    // A detached job that already finished is a legitimate id, so say so
    // instead of pretending the caller made it up.
    if state.jobs.lock().await.contains_key(&id) {
        return Ok(Json(CancelResponse {
            exec_id: id,
            cancelled: false,
        }));
    }
    Err(ApiError::not_found("exec id not found"))
}

fn cancel_group(state: &AppState, id: &str) -> bool {
    let pgid = {
        // The signal goes out under the same lock a guard takes to deregister,
        // so the group cannot be reaped — and its pid recycled — between the
        // lookup here and the kill.
        let table = lock_groups(&state.child_groups);
        match table.get(id).copied() {
            Some(pgid) => {
                signal_group(Some(pgid), term_signal());
                pgid
            }
            None => return false,
        }
    };
    let grace = state.kill_grace;
    let groups = state.child_groups.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        // Re-check under the lock: if the entry is gone the exec finished on
        // its own and this pgid may already belong to someone else.
        let table = lock_groups(&groups);
        if table.get(&id).copied() == Some(pgid) {
            signal_group(Some(pgid), kill_signal());
        }
    });
    true
}

fn reject_pty(req: &ExecRequest) -> Result<(), ApiError> {
    if req.pty.unwrap_or(false) {
        return Err(ApiError::invalid_request(
            "pty exec is not implemented; use POST /v1/exec/stream for incremental stdout/stderr",
        ));
    }
    Ok(())
}

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .any(|p| p.trim().starts_with("text/event-stream"))
}

fn admit(state: &AppState) -> Result<SlotGuard, ApiError> {
    match state.exec_slots.clone().try_acquire_owned() {
        Ok(permit) => {
            state.execs_in_flight.fetch_add(1, Ordering::Relaxed);
            state.execs_started.fetch_add(1, Ordering::Relaxed);
            Ok(SlotGuard {
                _permit: permit,
                inflight: state.execs_in_flight.clone(),
            })
        }
        Err(_) => Err(ApiError::busy(format!(
            "too many concurrent execs (max {})",
            state.max_concurrent_execs
        ))),
    }
}

fn prepare(state: &AppState, req: ExecRequest) -> Result<Prepared, ApiError> {
    let argv = match req.command {
        CommandSpec::Argv(parts) => {
            if parts.is_empty() {
                return Err(ApiError::invalid_request("command argv must not be empty"));
            }
            if parts[0].is_empty() {
                return Err(ApiError::invalid_request("command[0] must not be empty"));
            }
            parts
        }
        CommandSpec::Shell(script) => {
            if script.is_empty() {
                return Err(ApiError::invalid_request("command must not be empty"));
            }
            vec!["/bin/sh".to_string(), "-c".to_string(), script]
        }
    };

    let cwd_input = req.cwd.as_deref().unwrap_or("");
    let cwd = resolve_in_canonical_jail(&state.workspace, cwd_input)?;
    if !cwd.is_dir() {
        return Err(ApiError::invalid_request(format!(
            "cwd is not a directory: {}",
            cwd.display()
        )));
    }

    Ok(Prepared {
        argv,
        cwd,
        timeout: clamp_timeout(state, req.timeout_ms),
        env: req.env,
        stdin: req.stdin,
        exec_id: new_prefixed_id("exec"),
    })
}

async fn detach_run(state: AppState, prepared: Prepared) -> Result<Json<ExecResponse>, ApiError> {
    let slot = admit(&state)?;
    let exec_id = prepared.exec_id.clone();
    let cwd = prepared.cwd.display().to_string();
    {
        let mut jobs = state.jobs.lock().await;
        prune_jobs(&mut jobs);
        jobs.insert(
            exec_id.clone(),
            DetachedJob {
                status: "running",
                result: None,
            },
        );
    }
    let state_clone = state.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let id = prepared.exec_id.clone();
        let result = run_blocking(&state_clone, prepared).await;
        let mut jobs = state_clone.jobs.lock().await;
        match result {
            Ok(mut response) => {
                response.detached = true;
                response.status = Some("done");
                jobs.insert(
                    id,
                    DetachedJob {
                        status: "done",
                        result: Some(response),
                    },
                );
            }
            Err(err) => {
                jobs.insert(
                    id.clone(),
                    DetachedJob {
                        status: "done",
                        result: Some(ExecResponse {
                            stdout: String::new(),
                            stderr: err.to_string(),
                            exit_code: None,
                            timed_out: false,
                            duration_ms: 0,
                            truncated: false,
                            output_complete: false,
                            cwd: String::new(),
                            exec_id: id,
                            detached: true,
                            status: Some("error"),
                        }),
                    },
                );
            }
        }
    });
    Ok(Json(ExecResponse {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        truncated: false,
        output_complete: false,
        cwd,
        exec_id,
        detached: true,
        status: Some("running"),
    }))
}

fn prune_jobs(jobs: &mut HashMap<String, DetachedJob>) {
    if jobs.len() < MAX_DETACHED_JOBS {
        return;
    }
    let done: Vec<String> = jobs
        .iter()
        .filter(|(_, j)| j.status != "running")
        .map(|(k, _)| k.clone())
        .collect();
    for k in done {
        jobs.remove(&k);
        if jobs.len() < MAX_DETACHED_JOBS {
            break;
        }
    }
}

async fn run_blocking(state: &AppState, prepared: Prepared) -> Result<ExecResponse, ApiError> {
    let exec_id = prepared.exec_id.clone();
    let cwd_display = prepared.cwd.display().to_string();
    let mut child = spawn_child(
        prepared.argv,
        &prepared.cwd,
        prepared.env,
        prepared.stdin.is_some(),
    )?;
    if let Some(data) = prepared.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(data.as_bytes())
                .await
                .map_err(|err| ApiError::io(err.to_string()))?;
            stdin
                .shutdown()
                .await
                .map_err(|err| ApiError::io(err.to_string()))?;
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::internal("missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("missing stderr pipe"))?;

    let max = state.max_output_bytes;
    let mut guard = ChildGuard::adopt(child, &exec_id, state);
    let mut out = Capped::default();
    let mut err = Capped::default();
    let started = Instant::now();
    let mut saw_eof = false;
    let waited;
    {
        // The readers are futures this stack frame owns, not detached tasks. A
        // detached task parked on a pipe that a background grandchild holds
        // open is never woken again, and dropping its `JoinHandle` detaches
        // rather than cancels — that is how every such call used to leak two
        // descriptors and two tasks for the lifetime of the daemon.
        let mut drain = std::pin::pin!(async {
            tokio::join!(
                read_into(stdout, max, &mut out),
                read_into(stderr, max, &mut err)
            );
        });
        let mut waiter = std::pin::pin!(guard.wait(prepared.timeout));
        waited = loop {
            tokio::select! {
                res = &mut waiter => break res,
                _ = &mut drain, if !saw_eof => saw_eof = true,
            }
        };
        if !saw_eof {
            // The child is gone but something still holds the write ends. Take
            // what is already buffered and stop, rather than block on an EOF
            // that will not come and then throw the bytes away.
            saw_eof = tokio::time::timeout(POST_EXIT_DRAIN, &mut drain)
                .await
                .is_ok();
        }
    }
    let (timed_out, exit_code) = waited?;
    let duration_ms = started.elapsed().as_millis() as u64;

    Ok(ExecResponse {
        truncated: out.truncated || err.truncated || !saw_eof,
        output_complete: saw_eof,
        stdout: bytes_to_string(out.bytes),
        stderr: bytes_to_string(err.bytes),
        exit_code,
        timed_out,
        duration_ms,
        cwd: cwd_display,
        exec_id,
        detached: false,
        status: None,
    })
}

async fn stream_child(
    prepared: Prepared,
    state: AppState,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<(), ApiError> {
    let exec_id = prepared.exec_id.clone();
    let cwd_display = prepared.cwd.display().to_string();
    let mut child = spawn_child(
        prepared.argv,
        &prepared.cwd,
        prepared.env,
        prepared.stdin.is_some(),
    )?;
    if let Some(data) = prepared.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(data.as_bytes())
                .await
                .map_err(|err| ApiError::io(err.to_string()))?;
            stdin
                .shutdown()
                .await
                .map_err(|err| ApiError::io(err.to_string()))?;
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::internal("missing stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("missing stderr pipe"))?;

    let max = state.max_output_bytes;
    let mut guard = ChildGuard::adopt(child, &exec_id, &state);
    let mut out_trunc = false;
    let mut err_trunc = false;
    let started = Instant::now();
    let mut saw_eof = false;
    let waited;
    {
        let mut drain = std::pin::pin!(async {
            tokio::join!(
                pump_stream(stdout, max, true, tx, &mut out_trunc),
                pump_stream(stderr, max, false, tx, &mut err_trunc)
            );
        });
        let mut waiter = std::pin::pin!(guard.wait(prepared.timeout));
        // The response body stream — and with it the receiver — is dropped
        // when the client goes away. Without this arm the child would keep
        // running to its timeout with nobody left to read it.
        let mut client_gone = std::pin::pin!(tx.closed());
        waited = loop {
            tokio::select! {
                res = &mut waiter => break Some(res),
                _ = &mut client_gone => break None,
                _ = &mut drain, if !saw_eof => saw_eof = true,
            }
        };
        if waited.is_some() && !saw_eof {
            saw_eof = tokio::time::timeout(POST_EXIT_DRAIN, &mut drain)
                .await
                .is_ok();
        }
    }
    let Some(waited) = waited else {
        // Client gone. Returning here drops the guard, which terminates the
        // process group instead of orphaning it.
        return Ok(());
    };
    let (timed_out, exit_code) = waited?;
    let duration_ms = started.elapsed().as_millis() as u64;
    let _ = tx
        .send(StreamEvent::Exit {
            exec_id,
            exit_code,
            timed_out,
            duration_ms,
            truncated: out_trunc || err_trunc || !saw_eof,
            output_complete: saw_eof,
            cwd: cwd_display,
        })
        .await;
    Ok(())
}

async fn pump_stream<R: AsyncReadExt + Unpin>(
    mut reader: R,
    max: usize,
    is_stdout: bool,
    tx: &mpsc::Sender<StreamEvent>,
    truncated: &mut bool,
) {
    let mut seen = 0usize;
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if seen >= max {
                    *truncated = true;
                    continue;
                }
                let room = max.saturating_sub(seen);
                let take = n.min(room);
                seen += take;
                if take < n {
                    *truncated = true;
                }
                let data = String::from_utf8_lossy(&buf[..take]).into_owned();
                let evt = if is_stdout {
                    StreamEvent::Stdout { data }
                } else {
                    StreamEvent::Stderr { data }
                };
                if tx.send(evt).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn spawn_child(
    argv: Vec<String>,
    cwd: &std::path::Path,
    env: Option<HashMap<String, String>>,
    has_stdin: bool,
) -> Result<tokio::process::Child, ApiError> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if has_stdin {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    apply_child_env(&mut cmd, env.as_ref())?;
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn().map_err(|err| {
        ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "exec_failed",
            format!("failed to spawn command: {err}"),
        )
    })
}

fn term_signal() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGTERM
    }
    #[cfg(not(unix))]
    {
        15
    }
}

fn kill_signal() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGKILL
    }
    #[cfg(not(unix))]
    {
        9
    }
}

/// Names never handed to an exec child: the bearer and viewer secrets themselves and
/// the paths they can be delivered through, since a file the daemon could not
/// unlink would otherwise be one `cat` away.
fn strip_env_keys() -> impl Iterator<Item = &'static str> {
    SECRET_ENV_KEYS
        .iter()
        .chain(SECRET_FILE_ENV_KEYS.iter())
        .copied()
}

fn apply_child_env(
    cmd: &mut Command,
    extra: Option<&HashMap<String, String>>,
) -> Result<(), ApiError> {
    for key in strip_env_keys() {
        cmd.env_remove(key);
    }
    for (key, value) in DEFAULT_CHILD_ENV {
        let overridden = extra
            .map(|env| env.keys().any(|k| k.eq_ignore_ascii_case(key)))
            .unwrap_or(false);
        if !overridden {
            cmd.env(*key, *value);
        }
    }
    if let Some(env) = extra {
        for (key, value) in env {
            if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
                return Err(ApiError::invalid_request(
                    "invalid environment variable name or value",
                ));
            }
            if is_secret_key(key) {
                continue;
            }
            cmd.env(key, value);
        }
    }
    for key in strip_env_keys() {
        cmd.env_remove(key);
    }
    Ok(())
}

fn is_secret_key(key: &str) -> bool {
    strip_env_keys().any(|secret| secret.eq_ignore_ascii_case(key))
}

#[derive(Default)]
struct Capped {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Read `reader` to EOF, appending at most `max` bytes into `cap`.
///
/// Whatever has already been read stays in `cap` when this future is dropped,
/// which is what lets a caller stop reading a pipe a background process is
/// holding open and still return the foreground output.
async fn read_into<R: AsyncReadExt + Unpin>(mut reader: R, max: usize, cap: &mut Capped) {
    cap.bytes.reserve(8192.min(max));
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if cap.bytes.len() >= max {
                    cap.truncated = true;
                    continue;
                }
                let room = max.saturating_sub(cap.bytes.len());
                let take = n.min(room);
                cap.bytes.extend_from_slice(&buf[..take]);
                if take < n {
                    cap.truncated = true;
                }
            }
            Err(_) => break,
        }
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

fn clamp_timeout(state: &AppState, requested: Option<u64>) -> Duration {
    let requested = requested
        .map(Duration::from_millis)
        .unwrap_or(state.default_timeout);
    if requested < Duration::from_millis(1) {
        Duration::from_millis(1)
    } else if requested > state.max_timeout {
        state.max_timeout
    } else {
        requested
    }
}

/// Signal a whole process group. Each exec gets its own group via
/// `process_group(0)`, so this can never reach a process the box did not start.
fn signal_group(pgid: Option<i32>, sig: i32) {
    #[cfg(unix)]
    if let Some(pgid) = pgid {
        // Safety: `kill(2)` with a negative pid targets a process group; this
        // one was created for this exec and is still unreaped, so the id
        // cannot have been recycled.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;

    #[test]
    fn clamp_respects_max() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = AppState::for_test(dir.path().canonicalize().unwrap(), "t");
        let mut state = state;
        state.max_timeout = Duration::from_secs(60);
        state.default_timeout = Duration::from_secs(30);
        assert_eq!(
            clamp_timeout(&state, Some(120_000)),
            Duration::from_secs(60)
        );
        assert_eq!(clamp_timeout(&state, Some(0)), Duration::from_millis(1));
    }

    #[test]
    fn secret_keys_are_detected() {
        assert!(is_secret_key("BOX_TOKEN"));
        assert!(is_secret_key("box_token"));
        assert!(is_secret_key("BOX_HOST_TOKEN"));
        assert!(is_secret_key("BOX_VIEWER_PASSWORD"));
        assert!(is_secret_key("BOX_TOKEN_FILE"));
        assert!(is_secret_key("box_viewer_password_file"));
        assert!(is_secret_key("BOX_EGRESS_TUNNEL_BEARER"));
        assert!(is_secret_key("BOX_EGRESS_TUNNEL_BEARER_FILE"));
        assert!(!is_secret_key("PATH"));
        assert!(!is_secret_key("TERM"));
    }
}
