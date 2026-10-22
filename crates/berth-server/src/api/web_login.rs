//! The administrator's phone approves a sign-in to the web admin.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

use super::auth::Authenticated;
use crate::{
    AppState,
    db::now_ms,
    error::{ApiError, Problem},
    service::web_login::{self, WebLoginRequest},
};

/// Which browser asks to sign in with this code: the phone shows it before approving.
#[utoipa::path(
    get,
    path = "/v1/auth/web-login/{code}",
    tag = "auth",
    params(("code" = String, Path, description = "Code from the web admin's sign-in page")),
    responses(
        (status = OK, description = "The waiting sign-in", body = WebLoginRequest),
        (status = BAD_REQUEST, description = "`web_login_invalid`, `web_login_expired` or `web_login_used`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device lacks the `admin` scope", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn describe_web_login(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(code): Path<String>,
) -> Result<Json<WebLoginRequest>, ApiError> {
    Ok(Json(
        web_login::describe(&state.db, &device, &code, now_ms()).await?,
    ))
}

/// Approves the sign-in: the browser that shows the code is signed in as this phone's user.
#[utoipa::path(
    post,
    path = "/v1/auth/web-login/{code}/approve",
    tag = "auth",
    params(("code" = String, Path, description = "Code from the web admin's sign-in page")),
    responses(
        (status = NO_CONTENT, description = "Approved"),
        (status = BAD_REQUEST, description = "`web_login_invalid`, `web_login_expired` or `web_login_used`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device lacks the `admin` scope", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn approve_web_login(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(code): Path<String>,
) -> Result<StatusCode, ApiError> {
    web_login::approve(&state.db, &device, &code, now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}
