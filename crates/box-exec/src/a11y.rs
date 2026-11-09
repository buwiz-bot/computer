//! `POST /v1/cua/tree` and `POST /v1/cua/element`: a screen's accessibility tree, and an action on
//! one of its nodes by `ref`: AT-SPI's own press, focus or set-text, or a real click at its middle. The tree is read over that screen's own AT-SPI bus (`box_a11y::bus`),
//! which only box-exec and the screen's desktop may use, never a command's user.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use box_a11y::{A11yError, ElementRequest, ElementResponse, Scope, TreeRequest, TreeResponse};
use box_common::ApiError;
use box_cua::{ClickRequest, CuaConfig, CuaError};

use crate::cua::{map_err, OnScreen};
use crate::AppState;

pub fn map_a11y(err: A11yError) -> ApiError {
    let message = err.to_string();
    match err {
        A11yError::Unavailable(_) => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "a11y_unavailable", message)
        }
        A11yError::Invalid(_) => ApiError::invalid_request(message),
        A11yError::Stale(_) => ApiError::new(StatusCode::CONFLICT, "stale_ref", message),
        A11yError::Unsupported(_) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_action",
            message,
        ),
        A11yError::Refused(_) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "action_refused", message)
        }
        A11yError::Backend(_) => ApiError::new(StatusCode::BAD_GATEWAY, "a11y_backend", message),
    }
}

/// The screen, checked as every CUA route checks it.
fn screen(state: &AppState, on: &OnScreen) -> Result<CuaConfig, ApiError> {
    let cua = state.cua.on(on.display.as_deref()).map_err(map_err)?;
    if !cua.enabled {
        return Err(map_err(CuaError::Disabled));
    }
    Ok(cua)
}

pub async fn tree_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    req: Option<Json<TreeRequest>>,
) -> Result<Json<TreeResponse>, ApiError> {
    let req = req.map(|Json(req)| req).unwrap_or_default();
    let cua = screen(&state, &on)?;
    // X knows which window has the keyboard; AT-SPI often doesn't (box_a11y::tree::walk).
    let focus = match req.scope.unwrap_or_default() {
        Scope::FocusedWindow if req.limits().is_ok() => box_cua::keyboard_focus(&cua)
            .await
            .and_then(|focus| focus.window)
            .and_then(|window| window.title),
        _ => None,
    };
    box_a11y::tree(&cua.display, &req, &state.a11y, focus.as_deref())
        .await
        .map(Json)
        .map_err(map_a11y)
}

pub async fn element_handler(
    State(state): State<AppState>,
    Query(on): Query<OnScreen>,
    Json(req): Json<ElementRequest>,
) -> Result<Json<ElementResponse>, ApiError> {
    let cua = screen(&state, &on)?;
    let answer = box_a11y::element(&cua.display, &req, &state.a11y)
        .await
        .map_err(map_a11y)?;
    // `click` is a real click where the element is now, as /v1/cua/click makes it.
    if let Some(at) = answer.at {
        let click = ClickRequest {
            x: at.x,
            y: at.y,
            button: None,
        };
        box_cua::click(&cua, &click).await.map_err(map_err)?;
    }
    Ok(Json(answer))
}
