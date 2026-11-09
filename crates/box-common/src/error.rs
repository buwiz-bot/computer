use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::jail::JailError;

/// Wire error envelope used by every Phase 1 JSON error response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub status: u16,
    /// What a caller needs to act on this error, for the few that have any (`confirm_required`'s
    /// label and token). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ErrorResponse {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
                status: status.as_u16(),
                details: None,
            },
        }
    }
}

/// Application error that maps to a structured JSON body.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{message}")]
    Status {
        status: StatusCode,
        code: &'static str,
        message: String,
        details: Option<serde_json::Value>,
    },
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self::Status {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    /// This error with `details` in its body.
    pub fn with_details(self, value: serde_json::Value) -> Self {
        match self {
            Self::Status {
                status,
                code,
                message,
                ..
            } => Self::Status {
                status,
                code,
                message,
                details: Some(value),
            },
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token",
        )
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "io_error", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "busy", message)
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Status { status, .. } => *status,
        }
    }

    pub fn code(&self) -> &str {
        match self {
            Self::Status { code, .. } => code,
        }
    }

    pub fn into_response_body(&self) -> ErrorResponse {
        match self {
            Self::Status {
                status,
                code,
                message,
                details,
            } => {
                let mut body = ErrorResponse::new(*status, *code, message.clone());
                body.error.details = details.clone();
                body
            }
        }
    }
}

impl From<JailError> for ApiError {
    fn from(value: JailError) -> Self {
        match value {
            JailError::Empty => Self::invalid_request("path must not be empty"),
            JailError::Escape => Self::new(
                StatusCode::BAD_REQUEST,
                "path_escape",
                "path is outside the workspace jail",
            ),
            JailError::TooLong => Self::invalid_request("path is too long"),
            JailError::Invalid => Self::invalid_request("path contains invalid characters"),
            JailError::Io(err) => Self::io(err.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = self.into_response_body();
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthorized_wire_shape() {
        let err = ApiError::unauthorized();
        let body = err.into_response_body();
        assert_eq!(body.error.code, "unauthorized");
        assert_eq!(body.error.status, 401);
        let wire = serde_json::to_value(&body).unwrap();
        assert!(wire["error"].get("details").is_none());
    }

    #[test]
    fn details_ride_along_when_given() {
        let err = ApiError::new(StatusCode::CONFLICT, "confirm_required", "confirm it")
            .with_details(serde_json::json!({"label": "Pay"}));
        let wire = serde_json::to_value(err.into_response_body()).unwrap();
        assert_eq!(wire["error"]["details"]["label"], "Pay");
    }
}
