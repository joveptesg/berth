//! Device authentication: pairing, token refresh, self-revoke, and the bearer extractor.

use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Request, State},
    http::{StatusCode, header, request::Parts},
};
use berth_core::{AuthError, BerthError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use utoipa::ToSchema;

use crate::{
    AppState,
    db::now_ms,
    error::{ApiError, Problem},
    service::{
        devices, pairing,
        tokens::{self, DeviceAuth, TokenPair},
    },
};

/// JSON body whose parse errors are problem+json (`invalid_body`) like every other API error.
pub(crate) struct Body<T>(pub(crate) T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => {
                tracing::debug!(error = %rejection, "rejected request body");
                Err(BerthError::Validation("invalid_body".into()).into())
            }
        }
    }
}

/// The calling device, authenticated by `Authorization: Bearer <access token>`.
/// Rejects with `unauthenticated` or `device_revoked`.
pub(crate) struct Authenticated(pub(crate) DeviceAuth);

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(BerthError::Auth(AuthError::Unauthenticated))?;
        let device = tokens::authenticate(&state.db, &state.secret, token.trim(), now_ms()).await?;
        Ok(Self(device))
    }
}

/// `POST /v1/auth/pair` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PairRequest {
    /// Code from the QR or typed by hand (8 characters; case and separators are ignored).
    pairing_token: String,
    /// Human-readable device name, e.g. "Pixel 9".
    device_name: String,
    /// Lower-case platform: `android`, `ios`, …
    platform: String,
}

/// Tokens of a newly paired device.
#[derive(Serialize, ToSchema)]
pub(crate) struct PairResponse {
    /// Id of the new device.
    device_id: String,
    /// JWT for `Authorization: Bearer`.
    access_token: String,
    /// One-time refresh token; every refresh returns the next one.
    refresh_token: String,
    /// Seconds until the access token expires; refresh ahead of it.
    access_expires_in: i64,
}

/// `POST /v1/auth/refresh` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct RefreshRequest {
    /// The current refresh token.
    refresh_token: String,
}

/// A new token pair.
#[derive(Serialize, ToSchema)]
pub(crate) struct TokenResponse {
    /// JWT for `Authorization: Bearer`.
    access_token: String,
    /// The next refresh token; the presented one is spent.
    refresh_token: String,
    /// Seconds until the access token expires; refresh ahead of it.
    access_expires_in: i64,
}

impl From<TokenPair> for TokenResponse {
    fn from(pair: TokenPair) -> Self {
        Self {
            access_token: pair.access_token,
            refresh_token: pair.refresh_token,
            access_expires_in: pair.access_expires_in,
        }
    }
}

/// Exchange a pairing code for device tokens. Rate limited per client address.
#[utoipa::path(
    post,
    path = "/v1/auth/pair",
    tag = "auth",
    security(()),
    request_body = PairRequest,
    responses(
        (status = OK, description = "Device paired", body = PairResponse),
        (status = BAD_REQUEST, description = "`invalid_body`, `validation` (`platform_invalid`)", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`pairing_token_invalid`, `pairing_token_used` or `pairing_token_expired`", body = Problem, content_type = "application/problem+json"),
        (status = TOO_MANY_REQUESTS, description = "`rate_limited`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn pair(
    State(state): State<AppState>,
    Body(req): Body<PairRequest>,
) -> Result<Json<PairResponse>, ApiError> {
    let paired = pairing::pair(
        &state.db,
        &state.secret,
        &req.pairing_token,
        &req.device_name,
        &req.platform,
        now_ms(),
    )
    .await?;
    Ok(Json(PairResponse {
        device_id: paired.device_id,
        access_token: paired.tokens.access_token,
        refresh_token: paired.tokens.refresh_token,
        access_expires_in: paired.tokens.access_expires_in,
    }))
}

/// `POST /v1/auth/login` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct LoginRequest {
    /// Login the admin gave the listener (case is ignored).
    login: String,
    /// Password.
    password: String,
    /// Human-readable device name, e.g. "Pixel 9".
    device_name: String,
    /// Lower-case platform: `android`, `ios`, …
    platform: String,
}

/// Sign a device in with a listener's login and password, for phones that cannot scan a
/// pairing code (a store reviewer's). Same tokens as pairing; the server offers it when
/// `server-info` lists `password`. Rate limited per client address.
#[utoipa::path(
    post,
    path = "/v1/auth/login",
    tag = "auth",
    security(()),
    request_body = LoginRequest,
    responses(
        (status = OK, description = "Device signed in", body = PairResponse),
        (status = BAD_REQUEST, description = "`invalid_body`, `validation` (`platform_invalid`)", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`login_failed`: wrong login or password, or the login is off", body = Problem, content_type = "application/problem+json"),
        (status = TOO_MANY_REQUESTS, description = "`rate_limited`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn login(
    State(state): State<AppState>,
    Body(req): Body<LoginRequest>,
) -> Result<Json<PairResponse>, ApiError> {
    let paired = pairing::login(
        &state.db,
        &state.secret,
        (&req.login, &req.password),
        (&req.device_name, &req.platform),
        now_ms(),
    )
    .await?;
    Ok(Json(PairResponse {
        device_id: paired.device_id,
        access_token: paired.tokens.access_token,
        refresh_token: paired.tokens.refresh_token,
        access_expires_in: paired.tokens.access_expires_in,
    }))
}

/// Rotate the refresh token. A token presented again within 45 s gets the same new pair;
/// later reuse ends the device's session (`session_expired`).
#[utoipa::path(
    post,
    path = "/v1/auth/refresh",
    tag = "auth",
    security(()),
    request_body = RefreshRequest,
    responses(
        (status = OK, description = "New tokens", body = TokenResponse),
        (status = BAD_REQUEST, description = "`invalid_body`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`session_expired` (pair again, keep downloads) or `device_revoked` (wipe local data)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn refresh(
    State(state): State<AppState>,
    Body(req): Body<RefreshRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let pair = tokens::refresh(&state.db, &state.secret, &req.refresh_token, now_ms()).await?;
    Ok(Json(pair.into()))
}

/// Revoke the calling device (sign out). Its tokens stop working at once.
#[utoipa::path(
    post,
    path = "/v1/auth/revoke",
    tag = "auth",
    responses(
        (status = NO_CONTENT, description = "Device revoked"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn revoke(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
) -> Result<StatusCode, ApiError> {
    devices::revoke(&state.db, &device.tenant_id, &device.device_id, now_ms()).await?;
    Ok(StatusCode::NO_CONTENT)
}
