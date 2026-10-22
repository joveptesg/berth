use anyhow::Context;
use axum::{Json, extract::State};
use serde::Serialize;
use utoipa::ToSchema;

use super::auth::Authenticated;
use crate::{
    AppState,
    error::{ApiError, Problem},
};

/// The calling device and its user.
#[derive(Serialize, ToSchema)]
pub(crate) struct Me {
    /// User id.
    user_id: String,
    /// User name.
    user_name: String,
    /// Device id.
    device_id: String,
    /// Device scopes: `listen`, `upload`, `admin`, `feed`.
    scopes: Vec<String>,
}

/// Current user and device. Clients call it to check the session; errors tell them to
/// re-pair (`session_expired` on refresh) or wipe data (`device_revoked`).
#[utoipa::path(
    get,
    path = "/v1/me",
    tag = "auth",
    responses(
        (status = OK, description = "Current user and device", body = Me),
        (status = UNAUTHORIZED, description = "`unauthenticated` (refresh and retry) or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn me(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
) -> Result<Json<Me>, ApiError> {
    let user_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(&device.user_id)
        .fetch_one(&state.db)
        .await
        .context("load user")?;
    Ok(Json(Me {
        user_id: device.user_id,
        user_name,
        device_id: device.device_id,
        scopes: device.scopes,
    }))
}
