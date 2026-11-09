//! `/v1/browser/*`: a screen's Chromium over its DevTools pipe, through ultra-instinct
//! (`box_browser`). `?display=:N` picks the screen, as on `/v1/cua/*`. The pipe's other end is
//! this process's alone, never a command's: see docs/USERS.md.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use box_browser::{
    ActRequest, ActResponse, BrowserError, NavigateRequest, NavigateResponse, RunRequest,
    RunResponse, ShotResponse, SnapRequest, SnapResponse, StateRequest, StateResponse, TabsRequest,
    TabsResponse, WaitRequest, WaitResponse,
};
use box_common::ApiError;
use serde_json::json;

use crate::cua::{map_err, OnScreen};
use crate::AppState;

pub fn map_browser(err: BrowserError) -> ApiError {
    let message = err.to_string();
    match err {
        BrowserError::Disabled => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "browser_disabled", message)
        }
        BrowserError::Unavailable(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_unavailable",
            message,
        ),
        BrowserError::Invalid(_) => ApiError::invalid_request(message),
        BrowserError::Stale(_) => ApiError::new(StatusCode::CONFLICT, "stale_ref", message),
        BrowserError::ConfirmRequired { label, token } => {
            ApiError::new(StatusCode::CONFLICT, "confirm_required", message)
                .with_details(json!({ "label": label, "confirm": token }))
        }
        BrowserError::DomainNotAllowed(host) => {
            ApiError::new(StatusCode::FORBIDDEN, "domain_not_allowed", message)
                .with_details(json!({ "host": host }))
        }
        BrowserError::Refused(_) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "action_refused", message)
        }
        BrowserError::TargetGone(_) => ApiError::new(StatusCode::CONFLICT, "target_gone", message),
        BrowserError::DialogOpen(_) => ApiError::new(StatusCode::CONFLICT, "dialog_open", message),
        BrowserError::Internal(_) => ApiError::internal(message),
        BrowserError::Busy => ApiError::busy(message),
        BrowserError::Timeout(_) => {
            ApiError::new(StatusCode::GATEWAY_TIMEOUT, "browser_timeout", message)
        }
        BrowserError::Backend(_) => {
            ApiError::new(StatusCode::BAD_GATEWAY, "browser_backend", message)
        }
    }
}

/// The screen, checked as every CUA route checks it.
fn screen(state: &AppState, on: &OnScreen) -> Result<String, ApiError> {
    if !state.browser.enabled() {
        return Err(map_browser(BrowserError::Disabled));
    }
    let cua = state.cua.on(on.display.as_deref()).map_err(map_err)?;
    Ok(cua.display)
}

/// A body that may be left out: none, or only whitespace, is the defaults. Anything else must be
/// the request's JSON, whatever its `Content-Type` says, so a body is never quietly dropped.
fn optional_body<T: serde::de::DeserializeOwned + Default>(body: &Bytes) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|err| {
        ApiError::invalid_request(format!("the body isn't this route's JSON: {err}"))
    })
}

pub async fn snap_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    body: Bytes,
) -> Result<Json<SnapResponse>, ApiError> {
    let req: SnapRequest = optional_body(&body)?;
    let display = screen(&state, &on)?;
    state
        .browser
        .snap(&display, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn act_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    Json(req): Json<ActRequest>,
) -> Result<Json<ActResponse>, ApiError> {
    let display = screen(&state, &on)?;
    state
        .browser
        .act(&display, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn navigate_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    Json(req): Json<NavigateRequest>,
) -> Result<Json<NavigateResponse>, ApiError> {
    let display = screen(&state, &on)?;
    state
        .browser
        .navigate(&display, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn tabs_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    Json(req): Json<TabsRequest>,
) -> Result<Json<TabsResponse>, ApiError> {
    let display = screen(&state, &on)?;
    state
        .browser
        .tabs(&display, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn wait_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    body: Bytes,
) -> Result<Json<WaitResponse>, ApiError> {
    let req: WaitRequest = optional_body(&body)?;
    let display = screen(&state, &on)?;
    state
        .browser
        .wait(&display, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn shot_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
) -> Result<Json<ShotResponse>, ApiError> {
    let display = screen(&state, &on)?;
    state
        .browser
        .shot(&display)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn run_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    Json(req): Json<RunRequest>,
) -> Result<Json<RunResponse>, ApiError> {
    let display = screen(&state, &on)?;
    state
        .browser
        .run_goal(&display, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

pub async fn state_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    Json(req): Json<StateRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    let display = screen(&state, &on)?;
    // it reads the slots folder: off the async threads
    let owner = display.clone();
    let folder = tokio::task::spawn_blocking(move || state_owner(&owner))
        .await
        .map_err(|err| ApiError::internal(format!("the screen's owner wasn't found: {err}")))??;
    state
        .browser
        .state(&display, &folder, req)
        .await
        .map(Json)
        .map_err(map_browser)
}

/// Whose saved logins a screen keeps: `main` for the box's own, else the Bot holding it.
fn state_owner(display: &str) -> Result<String, ApiError> {
    let main = box_desktop::display_number(
        &std::env::var("BOX_SHARED_DISPLAY").unwrap_or_else(|_| ":1".to_string()),
    )
    .unwrap_or(1);
    if box_desktop::display_number(display) == Some(main) {
        return Ok("main".into());
    }
    box_desktop::screen_workspace(display, &box_desktop::slots_dir())
        .strip_prefix("agents/")
        .map(str::to_string)
        .ok_or_else(|| ApiError::not_found(format!("no Bot holds {display}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_is_its_json_or_nothing() {
        let wait: WaitRequest = optional_body(&Bytes::from_static(b"")).unwrap();
        assert!(wait.text.is_none());
        let wait: WaitRequest = optional_body(&Bytes::from_static(b" \n")).unwrap();
        assert!(wait.text.is_none());
        let wait: WaitRequest =
            optional_body(&Bytes::from_static(br#"{"text":"Order placed"}"#)).unwrap();
        assert_eq!(wait.text.as_deref(), Some("Order placed"));
        assert!(optional_body::<WaitRequest>(&Bytes::from_static(b"text=Order")).is_err());
    }
}
