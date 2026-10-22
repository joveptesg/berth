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
    AppState,
    error::ApiError,
    service::{auth, settings, setup},
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

pub(crate) async fn dashboard(State(state): State<AppState>) -> Page {
    render(&state, "dashboard.html", shell(&state, "dashboard"))
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

/// Settings are split into tabs, each a page of its own: `/settings` (server) and
/// `/settings/password`.
fn settings_tab(state: &AppState, tab: &str) -> Page {
    render(
        state,
        "settings.html",
        context! {
            ..shell(state, "settings"),
            ..context! { tab, settings => current_settings(state), locked => locked(state) },
        },
    )
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
