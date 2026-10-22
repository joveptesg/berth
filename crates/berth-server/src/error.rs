//! RFC 9457 `application/problem+json` error responses.

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use berth_core::{AuthError, BerthError};
use serde::Serialize;
use utoipa::ToSchema;

/// Problem details (RFC 9457).
#[derive(Debug, Serialize, ToSchema)]
pub struct Problem {
    /// URI identifying the problem type: `urn:berth:problem:<code>`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Short human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation of this occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Machine-readable code, e.g. `device_revoked` (wipe local data) or `session_expired` (re-pair).
    pub code: String,
}

/// Error returned by API handlers.
#[derive(Debug)]
pub struct ApiError(BerthError);

impl<E: Into<BerthError>> From<E> for ApiError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, detail) = match &self.0 {
            BerthError::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
            BerthError::Validation(msg) => {
                (StatusCode::BAD_REQUEST, "validation", Some(msg.clone()))
            }
            BerthError::Auth(auth) => (
                StatusCode::UNAUTHORIZED,
                match auth {
                    AuthError::Unauthenticated => "unauthenticated",
                    AuthError::DeviceRevoked => "device_revoked",
                    AuthError::SessionExpired => "session_expired",
                },
                None,
            ),
            BerthError::Internal(err) => {
                tracing::error!(error = ?err, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal", None)
            }
        };
        let problem = Problem {
            kind: format!("urn:berth:problem:{code}"),
            // Internal details stay in the log.
            title: match self.0 {
                BerthError::Internal(_) => "internal server error".to_owned(),
                BerthError::Validation(_) => "invalid request".to_owned(),
                ref err => err.to_string(),
            },
            status: status.as_u16(),
            detail,
            code: code.to_owned(),
        };
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(problem),
        )
            .into_response()
    }
}
