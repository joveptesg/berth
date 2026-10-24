//! Users with phone pairing, and devices: thin wrappers over `crate::service`.

use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use minijinja::{Value, context};
use qrcode::{QrCode, render::svg};
use serde::Deserialize;

use super::{
    pages::{Page, form_error, render, render_form, shell},
    redirect,
    session::AdminSession,
};
use crate::{
    AppState,
    db::now_ms,
    error::ApiError,
    service::{
        books::{self, BookQuery, PolicyFilter},
        devices, languages,
        pairing::{self, PairingStatus},
        settings, users,
    },
};
use berth_core::BerthError;

fn serde(value: impl serde::Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

// ── Users and phone pairing ──────────────────────────────

#[derive(Deserialize)]
pub(crate) struct UserForm {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
pub(crate) struct UsersQuery {
    /// Open the QR panel of this user (link without JavaScript, or after adding a listener).
    connect: Option<String>,
}

/// QR addresses: LAN and WAN from settings; without them, the address this page was opened at.
fn qr_urls(state: &AppState, headers: &HeaderMap) -> (Vec<String>, bool) {
    let configured = settings::server_urls(&state.config.load());
    if !configured.is_empty() {
        return (configured, false);
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    (vec![format!("{scheme}://{host}")], true)
}

/// The QR as inline SVG; the payload is what the app expects:
/// `{"server_urls": [lan, wan], "pairing_token": "<code>"}`.
fn qr_svg(urls: &[String], code: &str) -> anyhow::Result<String> {
    let payload = serde_json::json!({ "server_urls": urls, "pairing_token": code }).to_string();
    qr(&payload)
}

/// A QR code of `payload` as inline SVG.
pub(super) fn qr(payload: &str) -> anyhow::Result<String> {
    let svg = QrCode::new(payload.as_bytes())?
        .render::<svg::Color<'_>>()
        .min_dimensions(232, 232)
        .dark_color(svg::Color("#0e1014"))
        .light_color(svg::Color("#ffffff"))
        .build();
    // Inline in HTML: drop the XML declaration.
    Ok(svg
        .find("<svg")
        .map_or(svg.as_str(), |start| &svg[start..])
        .to_owned())
}

/// Issues a code for `user_id`: the context of the QR panel.
async fn code_panel(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    user_id: &str,
) -> Result<Value, ApiError> {
    let now = now_ms();
    let code = pairing::create(&state.db, tenant_id, user_id, now).await?;
    let (urls, fallback) = qr_urls(state, headers);
    let qr = Value::from_safe_string(qr_svg(&urls, &code.code)?);
    Ok(context! {
        id => code.id,
        code => code.code,
        qr,
        urls,
        fallback,
        user_id,
        expires_in => (code.expires_at - now) / 1000,
    })
}

/// The users list; `open` gets a fresh QR panel under its row.
async fn users_ctx(
    state: &AppState,
    headers: &HeaderMap,
    session: &AdminSession,
    open: Option<&str>,
) -> Result<Value, ApiError> {
    let tenant_id = &session.0.tenant_id;
    let list = users::list(&state.db, tenant_id).await?;
    let open = open.filter(|id| list.iter().any(|u| u.id == *id));
    let pairing = match open {
        Some(user_id) => Some(code_panel(state, headers, tenant_id, user_id).await?),
        None => None,
    };
    Ok(context! { users => serde(list), pairing, ..shell(state, "users") })
}

pub(crate) async fn users_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Query(query): Query<UsersQuery>,
) -> Page {
    let ctx = users_ctx(&state, &headers, &session, query.connect.as_deref()).await?;
    render(&state, "users.html", ctx)
}

/// Adds a listener and opens the QR panel for their phone right away.
pub(crate) async fn users_create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<UserForm>,
) -> Page {
    let (ctx, error) =
        match users::create_listener(&state.db, &session.0.tenant_id, &form.name).await {
            Ok(id) => {
                if !super::is_htmx(&headers) {
                    return Ok(redirect(&headers, &format!("/users?connect={id}")));
                }
                (
                    users_ctx(&state, &headers, &session, Some(&id)).await?,
                    None,
                )
            }
            Err(err) => (
                users_ctx(&state, &headers, &session, None).await?,
                Some(form_error(err)?),
            ),
        };
    let name = error.as_ref().map(|_| form.name);
    let ctx = context! { error, name, ..ctx };
    render_form(
        &state,
        &headers,
        "users.html",
        "partials/users_section.html",
        ctx,
    )
}

/// A listener's login for apps and the books they see.
#[derive(Deserialize)]
pub(crate) struct UserSettingsForm {
    /// Checkbox: `on` when apps may sign in with the login.
    #[serde(default)]
    login_on: Option<String>,
    #[serde(default)]
    login: String,
    /// Empty keeps the saved password.
    #[serde(default)]
    password: String,
    /// `all` or `selected`.
    #[serde(default)]
    access: String,
    /// Ids of the picked books, comma-separated.
    #[serde(default)]
    books: String,
    /// Language codes the user sees, comma-separated; empty for all.
    #[serde(default)]
    languages: String,
}

impl UserSettingsForm {
    fn picked(&self) -> Vec<String> {
        self.books
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// The settings dialog of a listener: as saved, or as `form` sent with its `error`.
async fn settings_ctx(
    state: &AppState,
    tenant_id: &str,
    user_id: &str,
    form: Option<&UserSettingsForm>,
    error: Option<String>,
) -> Result<Value, ApiError> {
    let user = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .find(|u| u.id == user_id && u.role == "member")
        .ok_or(BerthError::NotFound)?;
    let library = books::default_library_id(&state.db, tenant_id).await?;
    let list = books::list(
        &state.db,
        tenant_id,
        &PolicyFilter::admin(),
        &library,
        &BookQuery::default(),
    )
    .await?;
    let picked: Vec<String> = match form {
        Some(form) => form.picked(),
        None => users::access(&state.db, tenant_id, user_id)
            .await?
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_default(),
    };
    Ok(context! {
        user_settings => context! {
            access => form.map_or(user.book_access.clone(), |f| f.access.clone()),
            login_on => form.map_or(user.login.is_some(), |f| f.login_on.is_some()),
            login => form.map_or(user.login.clone().unwrap_or_default(), |f| f.login.clone()),
            has_password => user.login.is_some(),
            spoken => form.map_or(user.languages.clone(), |f| languages::parse_list(&f.languages)),
            user => serde(user),
            books => serde(list),
            picked,
            languages => languages::LANGUAGES.to_vec(),
            min_password => users::MIN_APP_PASSWORD_LEN,
            error,
        }
    })
}

/// The settings dialog, loaded into the users page.
pub(crate) async fn user_settings(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(user_id): Path<String>,
) -> Page {
    let ctx = settings_ctx(&state, &session.0.tenant_id, &user_id, None, None).await?;
    render(&state, "partials/user_settings.html", ctx)
}

/// Saves a listener's login and book access; the dialog comes back open with the error.
pub(crate) async fn user_settings_save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Form(form): Form<UserSettingsForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let saved = async {
        if form.login_on.is_some() {
            users::set_login(&state.db, tenant_id, &user_id, &form.login, &form.password).await?;
        } else {
            users::clear_login(&state.db, tenant_id, &user_id).await?;
        }
        let picked = form.picked();
        let books = (form.access == "selected").then_some(picked.as_slice());
        let spoken = languages::parse_list(&form.languages);
        users::set_access(&state.db, tenant_id, &user_id, books, &spoken).await
    }
    .await;
    let ctx = users_ctx(&state, &headers, &session, None).await?;
    let ctx = match saved {
        Ok(()) => {
            if !super::is_htmx(&headers) {
                return Ok(redirect(&headers, "/users"));
            }
            ctx
        }
        Err(err) => {
            let error = Some(form_error(err)?);
            let settings = settings_ctx(&state, tenant_id, &user_id, Some(&form), error).await?;
            context! { ..settings, ..ctx }
        }
    };
    render_form(
        &state,
        &headers,
        "users.html",
        "partials/users_section.html",
        ctx,
    )
}

/// QR panel under a user's row.
pub(crate) async fn user_connect(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Page {
    if !super::is_htmx(&headers) {
        return Ok(redirect(&headers, &format!("/users?connect={user_id}")));
    }
    let pairing = code_panel(&state, &headers, &session.0.tenant_id, &user_id).await?;
    render(&state, "partials/connect_code.html", context! { pairing })
}

/// Polled by the QR panel every 2 s: 204 while waiting, then the final panel and, once the
/// phone is connected, the user's new device count.
pub(crate) async fn pairing_status(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path((user_id, id)): Path<(String, String)>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let status = pairing::status(&state.db, tenant_id, &id, now_ms()).await?;
    if matches!(status, PairingStatus::Waiting { .. }) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let user = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .find(|u| u.id == user_id);
    render(
        &state,
        "partials/connect_result.html",
        context! { status => serde(status), user_id, user => serde(user) },
    )
}

// ── Devices ──────────────────────────────────────────────

pub(crate) async fn devices_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let list = devices::list(&state.db, &session.0.tenant_id).await?;
    render(
        &state,
        "devices.html",
        context! {
            devices => serde(list), ..shell(&state, "devices")
        },
    )
}

pub(crate) async fn devices_revoke(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    devices::revoke(&state.db, tenant_id, &id, now_ms()).await?;
    if !super::is_htmx(&headers) {
        return Ok(redirect(&headers, "/devices"));
    }
    // The row is swapped for nothing: revoked devices are not listed.
    Ok(().into_response())
}
