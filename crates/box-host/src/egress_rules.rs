//! `GET` and `PUT /v1/egress/rules`: the computer's browsing rules, changed while the box runs.
//!
//! box-host checks a new set, writes it to the rules file (its folder is box-host's user's alone;
//! box-keeper made it, and staged the set the box started with), and asks the proxy to read it
//! again on its loopback admin port. The answer is 200 only once the proxy reports the same
//! version in force. A set that doesn't check out is refused whole and changes nothing.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use box_common::{local_request, ApiError};
use box_egress::rules::{Exit, Rules, RulesSummary};
use serde::Serialize;
use serde_json::Value;

use crate::AppState;

/// The admin port is loopback and quick; a reload reads one small file.
const ADMIN_DEADLINE: Duration = Duration::from_secs(2);
const ADMIN_CAP: usize = box_egress::rules::MAX_RULES_BYTES + 4096;

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

fn not_running() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "egress_not_running",
        "the browser's network proxy isn't answering; the rules were saved and it reads them when it starts",
    )
}

pub(crate) async fn get_rules(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    if !state.egress.proxy {
        return Err(proxy_off());
    }
    let answer = local_request(
        &state.egress.admin_bind.to_string(),
        "GET",
        "/v1/rules",
        ADMIN_DEADLINE,
        ADMIN_CAP,
    )
    .await
    .map_err(|_| not_running())?;
    match answer.status {
        200 => serde_json::from_slice(&answer.body)
            .map(Json)
            .map_err(|_| not_running()),
        404 => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "egress_rules_unset",
            "no browsing rules are set on this box, so its browser reaches no site",
        )),
        _ => Err(not_running()),
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
    let path = state.egress.rules_path.clone();
    let json = rules.to_json();
    tokio::task::spawn_blocking(move || write_atomically(&path, json.as_bytes()))
        .await
        .map_err(|_| ApiError::internal("the rules file could not be written"))?
        .map_err(|err| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "egress_rules_unwritable",
                format!("the rules file could not be written: {}", err.kind()),
            )
        })?;
    let answer = local_request(
        &state.egress.admin_bind.to_string(),
        "POST",
        "/v1/rules/reload",
        ADMIN_DEADLINE,
        4096,
    )
    .await
    .map_err(|_| not_running())?;
    let loaded = serde_json::from_slice::<Value>(&answer.body)
        .ok()
        .and_then(|v| v.get("version").and_then(Value::as_u64));
    if answer.status != 200 || loaded != Some(rules.version) {
        return Err(not_running());
    }
    tracing::info!(version = rules.version, "egress rules applied");
    Ok(Json(Applied {
        applied: true,
        rules: RulesSummary::from(&rules),
    }))
}

/// Write beside the file, then rename over it: the proxy reads the old set or the new one, never
/// half of either.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the rules path has no folder"))?;
    let tmp = dir.join(format!(".rules.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&tmp, path)
}
