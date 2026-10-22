//! Web admin: server-rendered pages (`MiniJinja` + HTMX + Alpine) over the service layer.
//!
//! HTMX form posts get back only the fragment they replace; pages never fetch JSON.

mod access;
mod csrf;
mod i18n;
mod library;
mod pages;
mod session;
mod static_files;
mod storage;
pub(crate) mod templates;
mod upload;

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post, put},
};

/// A cover upload with its multipart framing.
const COVER_LIMIT: usize = 11 << 20;
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
        .route(
            "/login/phone",
            post(pages::login_phone_start).layer(limit(6, 10)),
        )
        .route("/login/phone/status", get(pages::login_phone_status))
        .route("/logout", post(pages::logout));

    let protected = Router::new()
        .route("/", get(pages::dashboard))
        .route(
            "/settings",
            get(pages::settings_page).post(pages::settings_server),
        )
        .route(
            "/settings/password",
            get(pages::settings_password_page).post(pages::settings_password),
        )
        .route("/settings/backup", get(pages::settings_backup_page))
        .route("/settings/restore", get(pages::settings_restore_page))
        .route("/settings/backup/download", get(pages::backup_download))
        .route(
            "/settings/backup/restore",
            post(pages::backup_restore_upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/settings/backup/{name}", get(pages::backup_file))
        .route(
            "/settings/backup/{name}/restore",
            post(pages::backup_restore),
        )
        .route("/users", get(access::users_page).post(access::users_create))
        .route("/users/{id}/connect", post(access::user_connect))
        .route("/users/{user}/pairing/{id}", get(access::pairing_status))
        .route("/devices", get(access::devices_page))
        .route("/devices/{id}/revoke", post(access::devices_revoke))
        .route("/storage", get(storage::page).post(storage::create))
        .route("/storage/{id}", post(storage::update))
        .route("/storage/{id}/verify", post(storage::verify))
        .route("/storage/{id}/default", post(storage::make_default))
        .route("/storage/{id}/delete", post(storage::delete))
        .route("/library", get(library::default_library))
        .route("/library/{id}", get(library::library))
        .route("/book/{id}", get(library::book).post(library::book_save))
        .route(
            "/book/{id}/cover",
            get(library::book_cover)
                .merge(post(library::book_cover_upload).layer(DefaultBodyLimit::max(COVER_LIMIT))),
        )
        .route("/book/{id}/delete", post(library::book_delete))
        .route("/upload", get(upload::page))
        .route("/upload/new", post(upload::create))
        .route(
            "/upload/{id}/files/{idx}",
            put(upload::put_file).layer(DefaultBodyLimit::disable()),
        )
        .route("/upload/{id}", get(upload::status))
        .route("/upload/{id}/complete", post(upload::complete))
        .route("/upload/{id}/save", post(upload::save))
        .route("/upload/{id}/discard", post(upload::discard))
        .route("/upload/{id}/cover", get(upload::cover))
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
fn redirect(headers: &HeaderMap, to: &str) -> Response {
    if is_htmx(headers) {
        let to = HeaderValue::from_str(to).expect("redirect targets are plain paths");
        (StatusCode::OK, [("hx-redirect", to)]).into_response()
    } else {
        Redirect::to(to).into_response()
    }
}
