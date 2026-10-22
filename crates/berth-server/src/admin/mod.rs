//! Web admin: server-rendered pages (`MiniJinja` + HTMX + Alpine) over the service layer.
//!
//! HTMX form posts get back only the fragment they replace; pages never fetch JSON.

mod csrf;
mod i18n;
mod pages;
mod session;
mod static_files;
pub(crate) mod templates;

use std::sync::Arc;

use axum::{
    Router,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

use crate::AppState;

/// Admin routes. Rate limits on password and setup posts are per peer IP: behind a reverse
/// proxy all clients share one bucket.
pub(crate) fn router(state: AppState) -> Router<AppState> {
    // One token per `per_second` seconds after the burst.
    let limit = |per_second, burst| {
        GovernorLayer::new(Arc::new(
            GovernorConfigBuilder::default()
                .per_second(per_second)
                .burst_size(burst)
                .finish()
                .expect("valid governor config"),
        ))
    };

    let public = Router::new()
        .route(
            "/setup",
            get(pages::setup_page).merge(post(pages::setup_submit).layer(limit(6, 10))),
        )
        .route(
            "/login",
            get(pages::login_page).merge(post(pages::login_submit).layer(limit(12, 5))),
        )
        .route("/logout", post(pages::logout));

    let protected = Router::new()
        .route("/", get(pages::dashboard))
        .route(
            "/settings",
            get(pages::settings_page).post(pages::settings_server),
        )
        .route("/settings/password", post(pages::settings_password))
        .layer(middleware::from_fn_with_state(state, session::require));

    public
        .merge(protected)
        .layer(middleware::from_fn(csrf::check))
        .layer(middleware::from_fn(i18n::locale_layer))
        .route("/static/{*path}", get(static_files::serve))
}

/// Value of one cookie from the `Cookie` header.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('='))
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Full-page navigation: `HX-Redirect` for HTMX requests, 303 otherwise.
fn redirect(headers: &HeaderMap, to: &'static str) -> Response {
    if is_htmx(headers) {
        (
            StatusCode::OK,
            [("hx-redirect", HeaderValue::from_static(to))],
        )
            .into_response()
    } else {
        Redirect::to(to).into_response()
    }
}
