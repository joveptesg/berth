//! Admin session cookie and the middleware that requires it.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{
    AppState,
    error::ApiError,
    service::{auth, setup},
};

pub(crate) const COOKIE: &str = "berth_session";
const MAX_AGE_S: i64 = 30 * 24 * 60 * 60;

/// The signed-in admin, put into request extensions by [`require`].
#[derive(Clone)]
pub(crate) struct AdminSession(pub(crate) auth::Session);

/// Session token from the request cookie.
pub(crate) fn token(headers: &HeaderMap) -> Option<&str> {
    super::cookie(headers, COOKIE)
}

/// Redirects to `/login` without a valid session, or to the setup wizard while the server is
/// not set up.
pub(crate) async fn require(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let session = match token(req.headers()) {
        Some(token) => match auth::session(&state.db, token).await {
            Ok(session) => session,
            Err(err) => return ApiError::from(err).into_response(),
        },
        None => None,
    };
    match session {
        Some(session) => {
            req.extensions_mut().insert(AdminSession(session));
            next.run(req).await
        }
        None => match setup::is_configured(&state.db).await {
            Ok(true) => super::redirect(req.headers(), "/login"),
            Ok(false) => super::redirect(req.headers(), "/setup"),
            Err(err) => ApiError::from(err).into_response(),
        },
    }
}

/// `Set-Cookie` opening a session; `Secure` when the request came over TLS (directly or via a
/// proxy setting `X-Forwarded-Proto`).
pub(crate) fn set_cookie(req_headers: &HeaderMap, token: &str) -> HeaderValue {
    cookie_header(req_headers, token, MAX_AGE_S)
}

pub(crate) fn clear_cookie(req_headers: &HeaderMap) -> HeaderValue {
    cookie_header(req_headers, "", 0)
}

fn cookie_header(req_headers: &HeaderMap, value: &str, max_age: i64) -> HeaderValue {
    let secure = req_headers
        .get("x-forwarded-proto")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"https"));
    let cookie = format!(
        "{COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::try_from(cookie).expect("token is base64url")
}
