//! Sign-in of a desktop app with a device code: the app shows a short code, an administrator
//! allows it on `/activate` in the web admin, the app polls for its tokens.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::auth::Body;
use crate::{
    AppState,
    db::now_ms,
    error::{ApiError, Problem},
    service::device_auth::{self, INTERVAL_S, Status},
};

/// `POST /v1/auth/device` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DeviceAuthRequest {
    /// Human-readable device name, e.g. "Alex's laptop".
    device_name: String,
    /// Lower-case platform: `windows`, `macos`, `linux`, …
    platform: String,
}

/// A started sign-in.
#[derive(Serialize, ToSchema)]
pub(crate) struct DeviceAuthStart {
    /// Secret of the app for `POST /v1/auth/device/token`; never shown.
    device_code: String,
    /// Code to show (8 characters; shown as two groups of four). The administrator confirms it
    /// at `/activate?code=<user_code>` on this server.
    user_code: String,
    /// Seconds until both codes expire.
    expires_in: i64,
    /// Seconds to wait between polls.
    interval: i64,
}

/// `POST /v1/auth/device/token` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DeviceTokenRequest {
    /// The `device_code` from `POST /v1/auth/device`.
    device_code: String,
}

/// Where the sign-in stands.
#[derive(Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DeviceTokenResponse {
    /// Not decided yet: poll again after `interval`.
    Waiting {
        /// Seconds until the code expires.
        expires_in: i64,
    },
    /// The administrator refused the sign-in.
    Denied,
    /// Expired, unknown or already used: start again.
    Expired,
    /// Allowed: a new device and its first tokens, as `POST /v1/auth/pair` gives.
    SignedIn {
        /// Id of the new device.
        device_id: String,
        /// JWT for `Authorization: Bearer`.
        access_token: String,
        /// One-time refresh token; every refresh returns the next one.
        refresh_token: String,
        /// Seconds until the access token expires; refresh ahead of it.
        access_expires_in: i64,
    },
}

/// Start a device-code sign-in (desktop apps). Rate limited per client address.
#[utoipa::path(
    post,
    path = "/v1/auth/device",
    tag = "auth",
    security(()),
    request_body = DeviceAuthRequest,
    responses(
        (status = OK, description = "Codes to show and poll with", body = DeviceAuthStart),
        (status = BAD_REQUEST, description = "`invalid_body`, `validation` (`platform_invalid`)", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`: the server is not set up", body = Problem, content_type = "application/problem+json"),
        (status = TOO_MANY_REQUESTS, description = "`rate_limited`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn start_device_auth(
    State(state): State<AppState>,
    Body(req): Body<DeviceAuthRequest>,
) -> Result<Json<DeviceAuthStart>, ApiError> {
    let now = now_ms();
    let started = device_auth::start(&state.db, &req.device_name, &req.platform, now).await?;
    Ok(Json(DeviceAuthStart {
        device_code: started.device_code,
        user_code: started.user_code,
        expires_in: (started.expires_at - now) / 1000,
        interval: INTERVAL_S,
    }))
}

/// Poll a device-code sign-in; once allowed, the tokens come back once. Rate limited per
/// client address.
#[utoipa::path(
    post,
    path = "/v1/auth/device/token",
    tag = "auth",
    security(()),
    request_body = DeviceTokenRequest,
    responses(
        (status = OK, description = "Where the sign-in stands", body = DeviceTokenResponse),
        (status = BAD_REQUEST, description = "`invalid_body`", body = Problem, content_type = "application/problem+json"),
        (status = TOO_MANY_REQUESTS, description = "`rate_limited`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn device_token(
    State(state): State<AppState>,
    Body(req): Body<DeviceTokenRequest>,
) -> Result<Json<DeviceTokenResponse>, ApiError> {
    let now = now_ms();
    Ok(Json(
        match device_auth::poll(&state.db, &state.secret, &req.device_code, now).await? {
            Status::Waiting { expires_at } => DeviceTokenResponse::Waiting {
                expires_in: (expires_at - now) / 1000,
            },
            Status::Denied => DeviceTokenResponse::Denied,
            Status::Expired => DeviceTokenResponse::Expired,
            Status::SignedIn(paired) => DeviceTokenResponse::SignedIn {
                device_id: paired.device_id,
                access_token: paired.tokens.access_token,
                refresh_token: paired.tokens.refresh_token,
                access_expires_in: paired.tokens.access_expires_in,
            },
        },
    ))
}
