//! Users, phone pairing (`/connect`) and devices: thin wrappers over `crate::service`.

use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
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
    service::{
        devices,
        pairing::{self, PairingStatus},
        settings, users,
    },
};

fn serde(value: impl serde::Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

// ── Users ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct UserForm {
    #[serde(default)]
    name: String,
}

async fn users_ctx(state: &AppState, session: &AdminSession) -> Result<Value, anyhow::Error> {
    let list = users::list(&state.db, &session.0.tenant_id).await?;
    Ok(context! { users => serde(list), ..shell(state, "users") })
}

pub(crate) async fn users_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    render(&state, "users.html", users_ctx(&state, &session).await?)
}

/// Creates a listener and goes straight to pairing their phone.
pub(crate) async fn users_create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<UserForm>,
) -> Page {
    match users::create_listener(&state.db, &session.0.tenant_id, &form.name).await {
        Ok(id) => Ok(redirect(&headers, &format!("/connect?user={id}"))),
        Err(err) => {
            let error = form_error(err)?;
            let ctx = context! { error, name => form.name, ..users_ctx(&state, &session).await? };
            render_form(
                &state,
                &headers,
                "users.html",
                "partials/user_form.html",
                ctx,
            )
        }
    }
}

// ── Connect ──────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConnectQuery {
    user: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConnectForm {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    new_name: String,
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

/// Issues a code for `user_id` and renders the QR panel.
async fn code_panel(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    user_id: &str,
) -> Result<Value, crate::error::ApiError> {
    let now = now_ms();
    let code = pairing::create(&state.db, tenant_id, user_id, now).await?;
    let (urls, fallback) = qr_urls(state, headers);
    let user_name = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .find(|u| u.id == user_id)
        .map(|u| u.name);
    let qr = Value::from_safe_string(qr_svg(&urls, &code.code)?);
    Ok(context! {
        pairing => context! {
            id => code.id,
            code => code.code,
            qr,
            urls,
            fallback,
            user_id,
            user_name,
            expires_in => (code.expires_at - now) / 1000,
        },
    })
}

pub(crate) async fn connect_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Query(query): Query<ConnectQuery>,
) -> Page {
    let list = users::list(&state.db, &session.0.tenant_id).await?;
    let selected = query.user.filter(|id| list.iter().any(|u| u.id == *id));
    // Coming from "add listener" or "connect a phone": show the code right away.
    let pairing = match &selected {
        Some(user_id) => code_panel(&state, &headers, &session.0.tenant_id, user_id).await?,
        None => context! {},
    };
    render(
        &state,
        "connect.html",
        context! { ..pairing, ..context! { users => serde(list), selected, ..shell(&state, "connect") } },
    )
}

pub(crate) async fn connect_create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<ConnectForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let user_id = if form.user_id == "new" {
        match users::create_listener(&state.db, tenant_id, &form.new_name).await {
            Ok(id) => id,
            Err(err) => {
                let error = form_error(err)?;
                let list = users::list(&state.db, tenant_id).await?;
                let mut response = render_form(
                    &state,
                    &headers,
                    "connect.html",
                    "partials/connect_form.html",
                    context! {
                        users => serde(list), selected => "new", new_name => form.new_name, error,
                        ..shell(&state, "connect")
                    },
                )?;
                // The form targets the QR panel; an error goes back into the form itself.
                let headers = response.headers_mut();
                headers.insert("hx-retarget", HeaderValue::from_static("#connect-form"));
                headers.insert("hx-reswap", HeaderValue::from_static("outerHTML"));
                return Ok(response);
            }
        }
    } else {
        form.user_id
    };
    let panel = code_panel(&state, &headers, tenant_id, &user_id).await?;
    if super::is_htmx(&headers) {
        // The form is re-rendered out of band so a new listener shows up in the list.
        let list = users::list(&state.db, tenant_id).await?;
        return render(
            &state,
            "partials/connect_swap.html",
            context! { users => serde(list), selected => user_id, ..panel },
        );
    }
    let list = users::list(&state.db, tenant_id).await?;
    render(
        &state,
        "connect.html",
        context! { ..panel, ..context! { users => serde(list), selected => user_id, ..shell(&state, "connect") } },
    )
}

/// Polled by the QR panel every 2 s: 204 while waiting, then the final panel.
pub(crate) async fn connect_status(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let status = pairing::status(&state.db, &session.0.tenant_id, &id, now_ms()).await?;
    if matches!(status, PairingStatus::Waiting { .. }) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    render(
        &state,
        "partials/connect_result.html",
        context! { status => serde(status) },
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
            devices => serde(list), current => session.0.device_id.clone(),
            ..shell(&state, "devices")
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
    if id == session.0.device_id {
        // Revoking this very browser signs it out.
        return Ok(redirect(&headers, "/login"));
    }
    if !super::is_htmx(&headers) {
        return Ok(redirect(&headers, "/devices"));
    }
    let device = devices::get(&state.db, tenant_id, &id).await?;
    render(
        &state,
        "partials/device_row.html",
        context! { device => serde(device), current => session.0.device_id.clone() },
    )
}
