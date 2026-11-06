//! `POST /v1/screen`: a Bot's screen, through `box-keeper`. Starting one takes root, for its users
//! and its X server's cookie, so `box-exec` doesn't do it. It passes a checked request over a
//! socket only its own user can open, and answers with what `box-screen` printed: JSON, with an
//! `error` when it failed.

use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use box_common::{ApiError, ScreenCall};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::AppState;

/// `box-screen` waits on its lock, then for an X server and a viewer.
const PATIENCE: Duration = Duration::from_secs(45);

#[derive(Debug, Deserialize)]
pub struct ScreenRequest {
    /// `up`, `down`, `forget`, `open` or `list`.
    pub action: String,
    pub bot: Option<String>,
    /// The page, for `open`.
    pub url: Option<String>,
}

pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<ScreenRequest>,
) -> Result<Response, ApiError> {
    let call = ScreenCall::new(&req.action, req.bot.as_deref(), req.url.as_deref())
        .ok_or_else(|| ApiError::invalid_request("that isn't a screen request"))?;
    let unavailable = |why: String| ApiError::new(StatusCode::BAD_GATEWAY, "keeper_failed", why);
    let answer = tokio::time::timeout(PATIENCE, async {
        let mut keeper = UnixStream::connect(&state.keeper_socket).await?;
        keeper.write_all(call.line().as_bytes()).await?;
        let mut answer = Vec::new();
        keeper.read_to_end(&mut answer).await?;
        Ok::<_, std::io::Error>(answer)
    })
    .await
    .map_err(|_| unavailable("box-screen took too long".to_string()))?
    .map_err(|err| unavailable(format!("box-keeper: {err}")))?;
    Ok(([(header::CONTENT_TYPE, "application/json")], answer).into_response())
}
