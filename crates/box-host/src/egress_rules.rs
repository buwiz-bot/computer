//! `GET` and `PUT /v1/egress/rules`: the computer's browsing rules, changed while the box runs.
//!
//! One `PUT` at a time ([`RulesTurn`]). Each checks the set, writes it to the rules file (its folder is
//! box-host's user's alone; box-prepare made it, and box-keeper put the staged set the box started
//! with there), and asks the proxy to read it again on its loopback admin port. The answer is 200
//! only once the proxy reports this set's version in force. If the proxy reports another version,
//! refuses the file, or doesn't answer, the file goes back to what it was, so the file is never
//! ahead of what's in force, and the answer says which it was.
//!
//! `GET` reads the lists from that file, and only while the proxy has the same version in force:
//! the proxy's own admin port, which has no login, gives counts alone.
//!
//! Where the rules in force came from is said in `GET /v1/egress` (`rules_origin`): `staged` (the
//! file the box started with), or `set` (a `PUT` since). A restart returns to the staged file:
//! `/run/box` is made afresh at every start.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use box_common::{local_request, ApiError};
use box_egress::rules::{Exit, Rules, RulesSummary};
use serde::Serialize;
use serde_json::{json, Value};

use crate::AppState;

/// The admin port is loopback and quick; a reload reads one small file.
const ADMIN_DEADLINE: Duration = Duration::from_secs(2);
const ADMIN_CAP: usize = 16 * 1024;

/// One `PUT` at a time (write, reload and, when it didn't take, put back, as one step), and the
/// version the last one put in force since this box started (0: none).
#[derive(Clone, Debug, Default)]
pub struct RulesTurn {
    turn: std::sync::Arc<tokio::sync::Mutex<()>>,
    origin: std::sync::Arc<std::sync::Mutex<Origin>>,
}

/// Where the rules in force came from, said as it happens, never worked out from numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Origin {
    /// what box-keeper put in place at start
    #[default]
    Staged,
    /// a `PUT` since, of this version
    Set(u64),
}

impl RulesTurn {
    fn set(&self, version: u64) {
        *self.origin.lock().unwrap_or_else(|e| e.into_inner()) = Origin::Set(version);
    }
}

/// `staged` or `set`, for the version in force; `None` with no rules.
pub(crate) fn origin(state: &AppState, in_force: Option<u64>) -> Option<&'static str> {
    let version = in_force?;
    let origin = *state
        .rules_turn
        .origin
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Some(match origin {
        Origin::Set(set) if set == version => "set",
        _ => "staged",
    })
}

#[derive(Serialize)]
pub(crate) struct Applied {
    applied: bool,
    rules: RulesSummary,
}

fn proxy_off() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "egress_proxy_off",
        "this box was made without the browser's network proxy (BOX_EGRESS_PROXY), so it has no rules to set; make the box again with it",
    )
}

fn not_running(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "egress_not_running",
        format!("the browser's network proxy isn't answering, so {what}"),
    )
}

fn unset() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "egress_rules_unset",
        "no browsing rules are set on this box, so its browser reaches no site",
    )
}

/// The version the proxy has in force, by its admin port: `Ok(None)` for none.
async fn in_force(state: &AppState) -> Result<Option<u64>, ()> {
    let answer = local_request(
        &state.egress.admin_bind.to_string(),
        "GET",
        "/v1/rules",
        ADMIN_DEADLINE,
        ADMIN_CAP,
    )
    .await
    .map_err(|_| ())?;
    match answer.status {
        200 => serde_json::from_slice::<Value>(&answer.body)
            .ok()
            .and_then(|v| v.get("version").and_then(Value::as_u64))
            .map(Some)
            .ok_or(()),
        404 => Ok(None),
        _ => Err(()),
    }
}

pub(crate) async fn get_rules(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    if !state.egress.proxy {
        return Err(proxy_off());
    }
    let _turn = state.rules_turn.turn.lock().await;
    let force = in_force(&state)
        .await
        .map_err(|()| not_running("what's in force can't be read"))?;
    let Some(version) = force else {
        return Err(unset());
    };
    let path = state.egress.rules_path.clone();
    let file = tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|bytes| Rules::parse(&bytes).ok());
    match file {
        Some(rules) if rules.version == version => Ok(Json(
            serde_json::from_str(&rules.to_json()).unwrap_or(Value::Null),
        )),
        _ => Err(ApiError::new(
            StatusCode::CONFLICT,
            "rules_superseded",
            "the rules file isn't the set the proxy has in force; set the rules again",
        )
        .with_details(json!({ "version": version }))),
    }
}

/// What a reload came to.
enum Reload {
    InForce,
    /// the proxy has another version in force, or none
    Other(Option<u64>),
    /// the proxy read the file and refused it
    Refused(String),
    /// no answer
    Down,
}

async fn reload(state: &AppState, version: u64) -> Reload {
    let Ok(answer) = local_request(
        &state.egress.admin_bind.to_string(),
        "POST",
        "/v1/rules/reload",
        ADMIN_DEADLINE,
        4096,
    )
    .await
    else {
        return Reload::Down;
    };
    let body = serde_json::from_slice::<Value>(&answer.body).unwrap_or(Value::Null);
    match answer.status {
        200 => match body.get("version").and_then(Value::as_u64) {
            Some(v) if v == version => Reload::InForce,
            other => Reload::Other(other),
        },
        404 => Reload::Other(None),
        422 => Reload::Refused(
            body.get("message")
                .and_then(Value::as_str)
                .unwrap_or("the proxy refused the rules")
                .to_owned(),
        ),
        _ => Reload::Down,
    }
}

pub(crate) async fn put_rules(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<Applied>, ApiError> {
    if !state.egress.proxy {
        return Err(proxy_off());
    }
    let mut rules = Rules::parse(&body)
        .map_err(|err| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_rules", err.0))?;
    if rules.exit == Exit::Tunnel && !state.egress.enabled {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "egress_tunnel_off",
            "this box was made without the tunnel (BOX_EGRESS_TUNNEL), so its browser can't go out through it; make the box again with it",
        ));
    }
    if rules.version == 0 {
        rules.version = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1)
            .max(1);
    }
    let _turn = state.rules_turn.turn.lock().await;
    let path = state.egress.rules_path.clone();
    let json = rules.to_json();
    let written = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            let before = std::fs::read(&path).ok();
            write_atomically(&path, json.as_bytes()).map(|()| before)
        })
        .await
        .map_err(|_| ApiError::internal("the rules file could not be written"))?
    };
    let before = written.map_err(|err| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "egress_rules_unwritable",
            format!("the rules file could not be written: {}", err.kind()),
        )
    })?;
    let outcome = reload(&state, rules.version).await;
    let applied = |rules: &Rules| {
        state.rules_turn.set(rules.version);
        tracing::info!(version = rules.version, "egress rules applied");
        Json(Applied {
            applied: true,
            rules: RulesSummary::from(rules),
        })
    };
    if matches!(outcome, Reload::InForce) {
        return Ok(applied(&rules));
    }
    // not told it's in force: the truth first, from the proxy itself. A reload that timed out may
    // well have taken; one that answered otherwise may not be the last word either
    // (a proxy that read the file and refused it has said what it holds: none of it)
    let truth = match outcome {
        Reload::Refused(_) => Ok(None),
        _ => in_force(&state).await,
    };
    if truth == Ok(Some(rules.version)) {
        return Ok(applied(&rules));
    }
    // not in force: the file goes back to what it was, never ahead of the proxy
    let restore = tokio::task::spawn_blocking(move || match before {
        Some(bytes) => write_atomically(&path, &bytes),
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    })
    .await;
    if !matches!(restore, Ok(Ok(()))) {
        tracing::warn!("the rules file could not be put back after a set that didn't take");
    }
    let Ok(_) = truth else {
        // the proxy can't be asked: the file is the last set it's known to have held, and what's
        // in force now isn't known
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "rules_state_unknown",
            "the browser's network proxy didn't answer, so whether the new rules are in force isn't known; the rules file is the last set it was known to hold. Ask again (GET /v1/egress) before relying on either",
        ));
    };
    // what the proxy holds isn't this set: it reads the file just put back
    let _ = reload(&state, 0).await;
    Err(match outcome {
        Reload::Other(version) => ApiError::new(
            StatusCode::CONFLICT,
            "rules_superseded",
            "the proxy has another set of rules in force; nothing was changed",
        )
        .with_details(json!({ "version": version })),
        Reload::Refused(why) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "rules_refused",
            format!("the proxy refused the rules ({why}); nothing was changed"),
        ),
        Reload::Down | Reload::InForce => not_running("nothing was changed"),
    })
}

/// Write beside the file, under a name no other write has, then rename over it: the proxy reads
/// the old set or the new one, never half of either.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the rules path has no folder"))?;
    let tmp = dir.join(format!(
        ".rules.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)?;
    let wrote = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)))
        .and_then(|()| std::fs::rename(&tmp, path));
    if wrote.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    wrote
}
