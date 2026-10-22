//! Page handlers: thin wrappers over `crate::service`.
//!
//! Form posts from HTMX get back the form fragment (errors, "saved"); without HTMX the whole
//! page is rendered.

use axum::{
    Extension, Form,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use berth_core::BerthError;
use minijinja::{Value, context};
use serde::Deserialize;

use super::{is_htmx, redirect, session, storage::StorageForm};
use crate::{
    AppState, backup,
    db::now_ms,
    error::ApiError,
    service::{auth, books, ingest, settings, setup, web_login},
};

pub(super) type Page = Result<Response, ApiError>;

pub(super) fn render(state: &AppState, name: &str, ctx: Value) -> Page {
    let html = state
        .templates
        .get_template(name)
        .and_then(|template| template.render(ctx))
        .map_err(|e| anyhow::anyhow!("template {name}: {e:#}"))?;
    Ok(Html(html).into_response())
}

/// Renders `fragment` for HTMX requests, `page` otherwise.
pub(super) fn render_form(
    state: &AppState,
    headers: &HeaderMap,
    page: &str,
    fragment: &str,
    ctx: Value,
) -> Page {
    render(state, if is_htmx(headers) { fragment } else { page }, ctx)
}

/// Splits a service error into a form error code or a real failure.
pub(super) fn form_error(err: BerthError) -> Result<String, ApiError> {
    match err {
        BerthError::Validation(code) => Ok(code),
        err => Err(err.into()),
    }
}

fn user_agent(headers: &HeaderMap) -> &str {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Signs the browser in and navigates to the dashboard.
fn signed_in(headers: &HeaderMap, token: &str) -> Response {
    let mut response = redirect(headers, "/");
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session::set_cookie(headers, token));
    response
}

fn locked(state: &AppState) -> Value {
    Value::from(minijinja::value::Serde(settings::locked(state)))
}

fn data_dir(state: &AppState) -> String {
    state.config.load().data_dir.display().to_string()
}

pub(super) fn shell(state: &AppState, page: &str) -> Value {
    context! { server_name => state.config.load().name.clone(), page }
}

// ── Setup ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct TokenQuery {
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
pub(crate) struct SetupForm {
    token: String,
    name: String,
    password: String,
    confirm: String,
    /// The storage step; without it (no JavaScript, old forms) a local folder `blobs`.
    #[serde(default)]
    storage_kind: String,
    #[serde(default)]
    storage_path: String,
    #[serde(default)]
    storage_endpoint: String,
    #[serde(default)]
    storage_bucket: String,
    #[serde(default)]
    storage_region: String,
    #[serde(default)]
    storage_path_style: Option<String>,
    #[serde(default)]
    storage_access_key_id: String,
    #[serde(default)]
    storage_secret_access_key: String,
}

impl SetupForm {
    /// The first profile is named after where it is; it can be renamed in `/storage`.
    fn storage(&self) -> StorageForm {
        let s3 = self.storage_kind == "s3";
        StorageForm {
            name: if s3 {
                self.storage_bucket.trim().to_owned()
            } else {
                "Local".into()
            },
            kind: if s3 { "s3" } else { "local" }.into(),
            path: match self.storage_path.trim() {
                "" => "blobs".into(),
                path => path.into(),
            },
            endpoint: self.storage_endpoint.clone(),
            bucket: self.storage_bucket.clone(),
            region: self.storage_region.clone(),
            path_style: self.storage_path_style.clone(),
            delivery_mode: String::new(),
            access_key_id: self.storage_access_key_id.clone(),
            secret_access_key: self.storage_secret_access_key.clone(),
        }
    }
}

pub(crate) async fn setup_page(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Page {
    if setup::is_configured(&state.db).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    // Without the token from the log the wizard does not exist.
    if !state.setup.matches(&q.token) {
        return Err(BerthError::NotFound.into());
    }
    let name = state.config.load().name.clone();
    render(
        &state,
        "setup.html",
        context! { token => q.token, name, locked => locked(&state), data_dir => data_dir(&state) },
    )
}

pub(crate) async fn setup_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Page {
    if setup::is_configured(&state.db).await? {
        return Ok(redirect(&headers, "/login"));
    }
    if !state.setup.matches(&form.token) {
        return Err(BerthError::NotFound.into());
    }
    let storage = form.storage();
    match setup::create_owner(
        &state,
        &form.name,
        &form.password,
        &form.confirm,
        &storage.input(),
    )
    .await
    {
        Ok(owner) => {
            let mut conn = state.db.acquire().await.map_err(anyhow::Error::from)?;
            let token = auth::create_session(
                &mut conn,
                &owner.tenant_id,
                &owner.user_id,
                user_agent(&headers),
            )
            .await?;
            Ok(signed_in(&headers, &token))
        }
        Err(BerthError::Conflict(_)) => Ok(redirect(&headers, "/login")),
        Err(err) => {
            let error = form_error(err)?;
            render_form(
                &state,
                &headers,
                "setup.html",
                "partials/setup_form.html",
                context! {
                    token => form.token, name => form.name, error,
                    locked => locked(&state), data_dir => data_dir(&state),
                    storage => storage.view(""),
                },
            )
        }
    }
}

// ── Login ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct LoginForm {
    password: String,
}

pub(crate) async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Page {
    if let Some(token) = session::token(&headers)
        && auth::session(&state.db, token).await?.is_some()
    {
        return Ok(Redirect::to("/").into_response());
    }
    let configured = setup::is_configured(&state.db).await?;
    render(
        &state,
        "login.html",
        context! { configured, ..shell(&state, "login") },
    )
}

pub(crate) async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Page {
    match auth::login(&state.db, &form.password, user_agent(&headers)).await {
        Ok(token) => Ok(signed_in(&headers, &token)),
        Err(err) => {
            let error = form_error(err)?;
            render_form(
                &state,
                &headers,
                "login.html",
                "partials/login_form.html",
                context! { error, configured => true, ..shell(&state, "login") },
            )
        }
    }
}

const LOGIN_COOKIE: &str = "berth_login";

/// Starts a sign-in with the phone: a code (and QR) for the administrator's phone, and the
/// claim of this browser in a short-lived cookie.
pub(crate) async fn login_phone_start(State(state): State<AppState>, headers: HeaderMap) -> Page {
    let started = web_login::start(&state.db, user_agent(&headers), now_ms()).await?;
    let payload = serde_json::json!({ "server_id": &*state.server_id, "web_login": started.code })
        .to_string();
    let qr = Value::from_safe_string(super::access::qr(&payload)?);
    let expires_in = (started.expires_at - now_ms()) / 1000;
    let mut response = render(
        &state,
        "partials/login_phone.html",
        context! { phone => context! { state => "waiting", code => started.code.clone(), qr, expires_in } },
    )?;
    let cookie = format!(
        "{LOGIN_COOKIE}={}.{}; Path=/login; Max-Age={}; HttpOnly; SameSite=Strict",
        started.code,
        started.claim,
        web_login::TTL_MS / 1000 + 120
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().map_err(anyhow::Error::from)?,
    );
    Ok(response)
}

/// Polled by the sign-in page: nothing while waiting, the session once the phone approves.
pub(crate) async fn login_phone_status(State(state): State<AppState>, headers: HeaderMap) -> Page {
    let (code, claim) = super::cookie(&headers, LOGIN_COOKIE)
        .and_then(|v| v.split_once('.'))
        .unwrap_or_default();
    match web_login::claim(&state.db, code, claim, user_agent(&headers), now_ms()).await? {
        web_login::Status::Waiting { .. } => Ok(axum::http::StatusCode::NO_CONTENT.into_response()),
        web_login::Status::Expired => render(
            &state,
            "partials/login_phone.html",
            context! { phone => context! { state => "expired" } },
        ),
        web_login::Status::SignedIn(token) => Ok(signed_in(&headers, &token)),
    }
}
pub(crate) async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Page {
    if let Some(token) = session::token(&headers) {
        auth::logout(&state.db, token).await?;
    }
    let mut response = redirect(&headers, "/login");
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session::clear_cookie(&headers));
    Ok(response)
}

// ── Dashboard ────────────────────────────────────────────

pub(crate) async fn dashboard(
    State(state): State<AppState>,
    Extension(session): Extension<session::AdminSession>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let library_id = books::default_library_id(&state.db, tenant_id).await?;
    let book_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM books WHERE tenant_id = $1 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await
    .map_err(anyhow::Error::from)?;
    let uploads = ingest::recent(&state.db, tenant_id, 5).await?;
    let jobs = crate::jobs::recent(&state.db, tenant_id, 6)
        .await
        .map_err(anyhow::Error::from)?;
    render(
        &state,
        "dashboard.html",
        context! {
            library_id, book_count,
            uploads => Value::from(minijinja::value::Serde(uploads)),
            jobs => Value::from(minijinja::value::Serde(jobs)),
            ..shell(&state, "dashboard")
        },
    )
}

// ── Settings ─────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ServerForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    lan_url: String,
    #[serde(default)]
    wan_url: String,
}

#[derive(Deserialize)]
pub(crate) struct PasswordForm {
    current: String,
    password: String,
    confirm: String,
}

fn current_settings(state: &AppState) -> Value {
    let config = state.config.load();
    let url = |u: &Option<url::Url>| u.as_ref().map(settings::display_url).unwrap_or_default();
    context! {
        name => config.name.clone(),
        lan_url => url(&config.lan_url),
        wan_url => url(&config.wan_url),
    }
}

/// Settings are split into tabs, each a page of its own: `/settings` (server),
/// `/settings/password` and `/settings/backup`.
fn settings_tab(state: &AppState, tab: &str) -> Page {
    render(state, "settings.html", backup_ctx(state, tab))
}

/// Settings page context with the backups and where the server key is kept.
fn backup_ctx(state: &AppState, tab: &str) -> Value {
    let data_dir = state.config.load().data_dir.clone();
    let key_path = if std::env::var_os("BERTH_SECRET_KEY").is_some() {
        "BERTH_SECRET_KEY".to_owned()
    } else {
        data_dir.join("secret.key").display().to_string()
    };
    context! {
        ..shell(state, "settings"),
        ..context! {
            tab, settings => current_settings(state), locked => locked(state),
            backups => Value::from(minijinja::value::Serde(backup::list(&data_dir))),
            keep => backup::KEEP, key_path,
        },
    }
}

/// The server key as a file, for keeping apart from the backups.
pub(crate) async fn secret_key_download(State(state): State<AppState>) -> Page {
    let mut response = attachment(
        "secret.key",
        hex::encode(state.secret.as_bytes()).into_bytes(),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain"),
    );
    Ok(response)
}

/// Deletes an automatic backup.
pub(crate) async fn backup_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Page {
    let path = backup::path_of(&state.config.load().data_dir, &name).ok_or(BerthError::NotFound)?;
    tokio::fs::remove_file(&path)
        .await
        .map_err(anyhow::Error::from)?;
    if !is_htmx(&headers) {
        return Ok(redirect(&headers, "/settings/backup"));
    }
    render(
        &state,
        "partials/settings_backup.html",
        backup_ctx(&state, "backup"),
    )
}

/// A fresh copy of the database as a download.
pub(crate) async fn backup_download(State(state): State<AppState>) -> Page {
    let tmp = state.config.load().data_dir.join("tmp");
    tokio::fs::create_dir_all(&tmp)
        .await
        .map_err(anyhow::Error::from)?;
    let name = format!("berth-{}.sqlite", now_ms());
    let path = tmp.join(&name);
    backup::snapshot(&state.db, &path).await?;
    let bytes = tokio::fs::read(&path).await.map_err(anyhow::Error::from);
    // The copy has the storage keys (encrypted): it must not linger in tmp.
    let _ = tokio::fs::remove_file(&path).await;
    Ok(attachment(&name, bytes?))
}

/// An automatic backup as a download.
pub(crate) async fn backup_file(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Page {
    let path = backup::path_of(&state.config.load().data_dir, &name).ok_or(BerthError::NotFound)?;
    let bytes = tokio::fs::read(&path).await.map_err(anyhow::Error::from)?;
    Ok(attachment(&name, bytes))
}

#[derive(Deserialize)]
pub(crate) struct RestoredQuery {
    #[serde(default)]
    restored: bool,
}

pub(crate) async fn settings_backup_page(State(state): State<AppState>) -> Page {
    settings_tab(&state, "backup")
}

/// The restore tab, with the result of a restore.
pub(crate) async fn settings_restore_page(
    State(state): State<AppState>,
    Query(query): Query<RestoredQuery>,
) -> Page {
    if !query.restored {
        return settings_tab(&state, "restore");
    }
    let backups = backup::list(&state.config.load().data_dir);
    render(
        &state,
        "settings.html",
        context! {
            ..shell(&state, "settings"),
            ..context! {
                tab => "restore", settings => current_settings(&state), locked => locked(&state),
                backups => Value::from(minijinja::value::Serde(backups)), keep => backup::KEEP,
                restored => true,
            },
        },
    )
}

/// Restores the database from an automatic backup.
pub(crate) async fn backup_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Page {
    let data_dir = state.config.load().data_dir.clone();
    let path = backup::path_of(&data_dir, &name).ok_or(BerthError::NotFound)?;
    restored(
        &state,
        &headers,
        backup::restore(&state.db, &data_dir, &path).await,
    )
    .await
}

/// Restores the database from an uploaded backup file.
pub(crate) async fn backup_restore_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> Page {
    let data_dir = state.config.load().data_dir.clone();
    let tmp = data_dir.join("tmp");
    tokio::fs::create_dir_all(&tmp)
        .await
        .map_err(anyhow::Error::from)?;
    let upload = tmp.join(format!("upload-{}.sqlite", now_ms()));
    let mut received = false;
    while let Ok(Some(mut field)) = multipart.next_field().await {
        if field.name() != Some("backup") {
            continue;
        }
        let mut file = tokio::fs::File::create(&upload)
            .await
            .map_err(anyhow::Error::from)?;
        while let Ok(Some(chunk)) = field.chunk().await {
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(anyhow::Error::from)?;
            received = true;
        }
    }
    let result = if received {
        backup::restore(&state.db, &data_dir, &upload).await
    } else {
        Err("restore_not_berth")
    };
    let _ = tokio::fs::remove_file(&upload).await;
    restored(&state, &headers, result).await
}

/// After a restore: settings come from the restored database; the page shows the outcome
/// (the browser may have to sign in again: sessions are the backup's).
async fn restored(
    state: &AppState,
    headers: &HeaderMap,
    result: Result<(), backup::RestoreError>,
) -> Page {
    match result {
        Ok(()) => {
            settings::reload(state).await?;
            Ok(redirect(headers, "/settings/restore?restored=true"))
        }
        Err(code) => {
            let backups = backup::list(&state.config.load().data_dir);
            render_form(
                state,
                headers,
                "settings.html",
                "partials/settings_restore.html",
                context! {
                    ..shell(state, "settings"),
                    ..context! {
                        tab => "restore", settings => current_settings(state),
                        locked => locked(state),
                        backups => Value::from(minijinja::value::Serde(backups)),
                        keep => backup::KEEP, restore_error => code,
                    },
                },
            )
        }
    }
}
fn attachment(name: &str, bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) async fn settings_page(State(state): State<AppState>) -> Page {
    settings_tab(&state, "server")
}

pub(crate) async fn settings_password_page(State(state): State<AppState>) -> Page {
    settings_tab(&state, "password")
}

pub(crate) async fn settings_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ServerForm>,
) -> Page {
    let input = settings::ServerSettingsInput {
        name: &form.name,
        lan_url: &form.lan_url,
        wan_url: &form.wan_url,
    };
    let (settings_ctx, error) = match settings::update(&state, &input).await {
        Ok(()) => (current_settings(&state), None),
        // Keep what the admin typed so the mistake can be fixed.
        Err(err) => (
            context! { name => form.name, lan_url => form.lan_url, wan_url => form.wan_url },
            Some(form_error(err)?),
        ),
    };
    render_form(
        &state,
        &headers,
        "settings.html",
        "partials/settings_server.html",
        context! {
            ..shell(&state, "settings"),
            ..context! {
                tab => "server", settings => settings_ctx, locked => locked(&state),
                saved => error.is_none(), error,
            },
        },
    )
}

pub(crate) async fn settings_password(
    State(state): State<AppState>,
    Extension(session::AdminSession(session)): Extension<session::AdminSession>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Page {
    let result = auth::change_password(
        &state.db,
        &session,
        &form.current,
        &form.password,
        &form.confirm,
    )
    .await;
    let error = match result {
        Ok(()) => None,
        Err(err) => Some(form_error(err)?),
    };
    render_form(
        &state,
        &headers,
        "settings.html",
        "partials/settings_password.html",
        context! {
            ..shell(&state, "settings"),
            ..context! {
                tab => "password", settings => current_settings(&state), locked => locked(&state),
                password_saved => error.is_none(), password_error => error,
            },
        },
    )
}
