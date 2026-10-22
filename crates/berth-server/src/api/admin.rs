//! Admin endpoints for devices with the `admin` scope.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::auth::{Authenticated, Body};
use crate::{
    AppState,
    db::now_ms,
    error::{ApiError, Problem},
    service::{pairing, settings},
};

/// `POST /v1/admin/pairing-tokens` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PairingTokenRequest {
    /// User the phone will belong to.
    user_id: String,
}

/// A pairing code and the QR payload around it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PairingTokenResponse {
    /// 8-character one-time code; the QR carries `{server_urls, pairing_token}`.
    pairing_token: String,
    /// Server addresses for the QR: home network first, then internet (from settings).
    server_urls: Vec<String>,
    /// Expiry, epoch milliseconds.
    expires_at: i64,
    /// Seconds until expiry.
    expires_in: i64,
}

/// Issue a one-time pairing code (lives 120 s).
#[utoipa::path(
    post,
    path = "/v1/admin/pairing-tokens",
    tag = "admin",
    request_body = PairingTokenRequest,
    responses(
        (status = CREATED, description = "Pairing code", body = PairingTokenResponse),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`: no such user", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn create_pairing_token(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Body(req): Body<PairingTokenRequest>,
) -> Result<(StatusCode, Json<PairingTokenResponse>), ApiError> {
    device.require("admin")?;
    let now = now_ms();
    let code = pairing::create(&state.db, &device.tenant_id, &req.user_id, now).await?;
    Ok((
        StatusCode::CREATED,
        Json(PairingTokenResponse {
            pairing_token: code.code,
            server_urls: settings::server_urls(&state.config.load()),
            expires_at: code.expires_at,
            expires_in: (code.expires_at - now) / 1000,
        }),
    ))
}
